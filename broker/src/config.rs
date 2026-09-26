use std::collections::HashMap;
use core::net::SocketAddr;
use std::path::Path;
use core::time::Duration;

use anyhow::{Context as _, bail};

/// Runtime settings. Process environment wins over `broker/.env`.
#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub management_bind: SocketAddr,
    pub target_host: String,
    pub target_port: u16,
    /// Lab VMs present a self-signed RDP certificate. `insecure` accepts it.
    pub tls_insecure: bool,
    /// Close a relay after this long with no bytes in either direction. Zero disables it.
    pub idle_timeout: Duration,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let file = dotenv_map(broker_dotenv_path());

        let bind = lookup(&file, "BIND_ADDR")
            .unwrap_or_else(|| "0.0.0.0:7171".to_owned())
            .parse()
            .context("BIND_ADDR must be host:port")?;

        let management_bind = lookup(&file, "MANAGEMENT_ADDR")
            .unwrap_or_else(|| "0.0.0.0:9090".to_owned())
            .parse()
            .context("MANAGEMENT_ADDR must be host:port")?;

        let target = lookup(&file, "RDP_TARGET")
            .context("RDP_TARGET is required (host:port of the lab VM). Set it in broker/.env")?;
        let (target_host, target_port) = split_host_port(&target)?;

        let tls_mode = lookup(&file, "RDP_TLS_VERIFY").unwrap_or_else(|| "insecure".to_owned());
        let tls_insecure = match tls_mode.as_str() {
            "insecure" => true,
            "strict" => false,
            other => bail!("RDP_TLS_VERIFY must be \"insecure\" or \"strict\", got {other}"),
        };

        let idle_timeout_secs: u64 = lookup(&file, "IDLE_TIMEOUT_SECS")
            .unwrap_or_else(|| "900".to_owned())
            .parse()
            .context("IDLE_TIMEOUT_SECS must be a whole number of seconds")?;

        Ok(Self {
            bind,
            management_bind,
            target_host,
            target_port,
            tls_insecure,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
        })
    }

    pub fn target_label(&self) -> String {
        format!("{}:{}", self.target_host, self.target_port)
    }

    pub fn allows(&self, host: &str, port: u16) -> bool {
        port == self.target_port && host.eq_ignore_ascii_case(&self.target_host)
    }
}

/// Process environment wins over values in the dotenv file.
fn lookup(file: &HashMap<String, String>, key: &str) -> Option<String> {
    std::env::var(key).ok().or_else(|| file.get(key).cloned())
}

/// Absolute path to `broker/.env` (git-ignored).
pub fn broker_dotenv_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".env")
}

/// Parse a dotenv-style file into a map (no shell expansion).
pub fn dotenv_map(path: impl AsRef<Path>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'').to_owned();
        map.insert(key.to_owned(), value);
    }
    map
}

pub fn split_host_port(value: &str) -> anyhow::Result<(String, u16)> {
    let value = value.trim();
    if value.is_empty() {
        bail!("destination is empty");
    }

    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .context("IPv6 destination must look like [addr]:port")?;
        (host, port)
    } else {
        value.rsplit_once(':').context("destination must be host:port")?
    };

    if host.is_empty() {
        bail!("destination host is empty");
    }
    let port: u16 = port
        .parse()
        .with_context(|| format!("destination port in {value:?} is not a u16"))?;
    Ok((host.to_owned(), port))
}
