//! Per-session STUN/TURN servers for Mode B, minted from a Cloudflare TURN key.
//!
//! The broker holds the key (`TURN_KEY_ID`, `TURN_API_TOKEN`) and calls [generate-ice-servers] for
//! every Mode B session, so each session gets credentials that expire after
//! `TURN_CREDENTIAL_TTL_SECS` (default 3600). The same servers configure webrtcbin and go to the
//! browser in the signaling handshake, so both peers can relay. The API token never leaves the
//! broker, and neither it nor the minted credentials are logged.
//!
//! [generate-ice-servers]: https://developers.cloudflare.com/realtime/turn/generate-credentials/

use core::fmt;
use core::time::Duration;

use anyhow::{Context as _, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::config::{broker_dotenv_path, dotenv_map};

const API_BASE: &str = "https://rtc.live.cloudflare.com/v1/turn/keys";
const API_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_TTL_SECS: u32 = 3600;
/// Cloudflare's upper bound for `ttl` (48 hours).
const MAX_TTL_SECS: u32 = 172_800;

/// One `RTCIceServer`, in the shape Cloudflare returns and the browser consumes.
/// `Debug` omits the credentials.
#[derive(Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl fmt::Debug for IceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls)
            .field("credential", &self.credential.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// ICE servers for one session. Empty means host candidates only.
#[derive(Clone, Debug, Default)]
pub struct IceServers(Vec<IceServer>);

impl IceServers {
    fn from_response(body: &[u8]) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        struct GenerateResponse {
            #[serde(rename = "iceServers")]
            ice_servers: Vec<IceServer>,
        }

        // The parse error is dropped: it can quote the minted credential.
        let response: GenerateResponse =
            serde_json::from_slice(body).map_err(|_| anyhow!("unexpected generate-ice-servers response body"))?;
        let servers: Vec<IceServer> = response
            .ice_servers
            .into_iter()
            .filter_map(|mut server| {
                // Cloudflare also lists port 53, which browsers block; those candidates only time out.
                server.urls.retain(|url| !is_port_53(url));
                (!server.urls.is_empty()).then_some(server)
            })
            .collect();
        let ice = Self(servers);
        ensure!(
            ice.turn_count() > 0,
            "generate-ice-servers returned no usable TURN server"
        );
        Ok(ice)
    }

    /// The servers to send to the browser as `RTCConfiguration.iceServers`.
    pub fn servers(&self) -> &[IceServer] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `stun://host:port` for webrtcbin's `stun-server` property (it takes only one).
    pub fn stun_server(&self) -> Option<String> {
        self.0
            .iter()
            .flat_map(|server| &server.urls)
            .find_map(|url| split_url(url).filter(|(scheme, _)| *scheme == "stun"))
            .map(|(_, rest)| {
                let host = rest.split_once('?').map_or(rest, |(host, _query)| host);
                format!("stun://{host}")
            })
    }

    /// URIs for webrtcbin's `add-turn-server` signal: `turn(s)://user:pass@host:port[?transport=…]`.
    ///
    /// These carry the credentials, so never log them.
    pub fn turn_uris(&self) -> Vec<String> {
        let mut uris = Vec::new();
        for server in &self.0 {
            let (Some(username), Some(credential)) = (&server.username, &server.credential) else {
                continue;
            };
            let userinfo = format!("{}:{}", percent_encode(username), percent_encode(credential));
            for url in &server.urls {
                if let Some((scheme @ ("turn" | "turns"), rest)) = split_url(url) {
                    uris.push(format!("{scheme}://{userinfo}@{rest}"));
                }
            }
        }
        uris
    }

    pub fn turn_count(&self) -> usize {
        self.turn_uris().len()
    }
}

/// Mints [`IceServers`] from a Cloudflare TURN key. `Debug` omits the API token.
pub struct TurnMinter {
    client: reqwest::Client,
    url: String,
    api_token: String,
    ttl_secs: u32,
}

impl TurnMinter {
    /// Fails when `TURN_KEY_ID` or `TURN_API_TOKEN` is unset, so the broker can disable Mode B.
    pub fn load() -> anyhow::Result<Self> {
        let file = dotenv_map(broker_dotenv_path());
        let lookup = |key: &str| {
            std::env::var(key)
                .ok()
                .or_else(|| file.get(key).cloned())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };

        let key_id = lookup("TURN_KEY_ID").context("TURN_KEY_ID is required for Mode B (Cloudflare TURN key ID)")?;
        ensure!(
            key_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "TURN_KEY_ID must be a Cloudflare TURN key ID"
        );
        let api_token = lookup("TURN_API_TOKEN")
            .context("TURN_API_TOKEN is required for Mode B (Cloudflare TURN key API token)")?;
        let ttl_secs: u32 = lookup("TURN_CREDENTIAL_TTL_SECS")
            .map_or(Ok(DEFAULT_TTL_SECS), |value| value.parse())
            .context("TURN_CREDENTIAL_TTL_SECS must be a whole number of seconds")?;
        ensure!(
            (60..=MAX_TTL_SECS).contains(&ttl_secs),
            "TURN_CREDENTIAL_TTL_SECS must be between 60 and {MAX_TTL_SECS}"
        );
        let client = reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .context("build Cloudflare TURN HTTP client")?;

        Ok(Self {
            client,
            url: format!("{API_BASE}/{key_id}/credentials/generate-ice-servers"),
            api_token,
            ttl_secs,
        })
    }

    pub fn ttl_secs(&self) -> u32 {
        self.ttl_secs
    }

    /// Mint fresh STUN/TURN servers valid for [`Self::ttl_secs`].
    pub async fn mint(&self) -> anyhow::Result<IceServers> {
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(format!(r#"{{"ttl":{}}}"#, self.ttl_secs))
            .send()
            .await
            .context("call Cloudflare generate-ice-servers")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Cloudflare generate-ice-servers returned {status}");
        }
        let body = response.bytes().await.context("read generate-ice-servers response")?;
        IceServers::from_response(&body)
    }
}

impl fmt::Debug for TurnMinter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnMinter")
            .field("url", &self.url)
            .field("ttl_secs", &self.ttl_secs)
            .finish_non_exhaustive()
    }
}

/// `("turn", "host:port?transport=udp")` from WebRTC's `turn:host:port?transport=udp` form.
fn split_url(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once(':')?;
    let scheme = match scheme {
        "stun" | "STUN" => "stun",
        "turn" | "TURN" => "turn",
        "turns" | "TURNS" => "turns",
        _ => return None,
    };
    Some((scheme, rest.strip_prefix("//").unwrap_or(rest)))
}

fn is_port_53(url: &str) -> bool {
    split_url(url)
        .map(|(_, rest)| rest.split_once('?').map_or(rest, |(host, _query)| host))
        .and_then(|host| host.rsplit_once(':'))
        .is_some_and(|(_, port)| port == "53")
}

/// Percent-encode a URI userinfo component; webrtcbin unescapes user and password separately.
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape of a Cloudflare generate-ice-servers response (credentials replaced).
    const RESPONSE: &str = r#"{"iceServers":[
        {"urls":["stun:stun.cloudflare.com:3478","stun:stun.cloudflare.com:53"]},
        {"urls":["turn:turn.cloudflare.com:3478?transport=udp","turn:turn.cloudflare.com:3478?transport=tcp",
                 "turns:turn.cloudflare.com:5349?transport=tcp","turn:turn.cloudflare.com:53?transport=udp"],
         "username":"user","credential":"p/ss+w=rd:x"}
    ]}"#;

    #[test]
    fn response_maps_to_webrtcbin_uris_without_port_53() {
        let ice = IceServers::from_response(RESPONSE.as_bytes()).unwrap();
        assert_eq!(ice.stun_server().as_deref(), Some("stun://stun.cloudflare.com:3478"));
        assert_eq!(
            ice.turn_uris(),
            [
                "turn://user:p%2Fss%2Bw%3Drd%3Ax@turn.cloudflare.com:3478?transport=udp",
                "turn://user:p%2Fss%2Bw%3Drd%3Ax@turn.cloudflare.com:3478?transport=tcp",
                "turns://user:p%2Fss%2Bw%3Drd%3Ax@turn.cloudflare.com:5349?transport=tcp",
            ]
        );
    }

    #[test]
    fn browser_shape_is_rtc_ice_server() {
        let ice = IceServers::from_response(RESPONSE.as_bytes()).unwrap();
        let json = serde_json::to_value(ice.servers()).unwrap();
        assert_eq!(
            json[0],
            serde_json::json!({ "urls": ["stun:stun.cloudflare.com:3478"] })
        );
        assert_eq!(json[1]["username"], "user");
        assert_eq!(json[1]["urls"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn response_without_turn_is_rejected() {
        let body = r#"{"iceServers":[{"urls":["stun:stun.cloudflare.com:3478"]}]}"#;
        assert!(IceServers::from_response(body.as_bytes()).is_err());
    }

    #[test]
    fn parse_error_does_not_echo_body() {
        let body = r#"{"iceServers":[{"urls":"turn:x","credential":"secret"}]}"#;
        let err = IceServers::from_response(body.as_bytes()).unwrap_err();
        assert!(!format!("{err:#}").contains("secret"));
    }

    #[test]
    fn debug_hides_credentials() {
        let ice = IceServers::from_response(RESPONSE.as_bytes()).unwrap();
        let debug = format!("{ice:?}");
        assert!(!debug.contains("p/ss") && !debug.contains("\"user\""));
    }

    #[test]
    fn empty_is_host_only() {
        let ice = IceServers::default();
        assert!(ice.is_empty());
        assert_eq!(ice.stun_server(), None);
        assert!(ice.turn_uris().is_empty());
    }
}
