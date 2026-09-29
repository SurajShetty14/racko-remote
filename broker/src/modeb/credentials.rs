//! Server-side RDP credentials for Mode B (bastion): the browser never supplies them.

use core::fmt;
use core::future::Future;
use core::pin::Pin;

use anyhow::Context as _;

use crate::config::{broker_dotenv_path, dotenv_map};

/// Credentials the broker uses to sign in to one target.
#[derive(Clone)]
pub struct RdpCredentials {
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
}

impl fmt::Debug for RdpCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .finish()
    }
}

/// Looks up credentials for an already allow-listed target.
///
/// Returns a boxed future so a per-target vault lookup can replace [`EnvCredentials`].
pub trait CredentialResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<RdpCredentials>> + Send + 'a>>;
}

/// One admin credential set for every target: `RDP_USERNAME`, `RDP_PASSWORD`, optional `RDP_DOMAIN`.
#[derive(Debug, Clone)]
pub struct EnvCredentials(RdpCredentials);

impl EnvCredentials {
    pub fn load() -> anyhow::Result<Self> {
        let file = dotenv_map(broker_dotenv_path());
        let lookup = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned());

        Ok(Self(RdpCredentials {
            username: lookup("RDP_USERNAME").context("RDP_USERNAME is required for Mode B")?,
            password: lookup("RDP_PASSWORD").context("RDP_PASSWORD is required for Mode B")?,
            domain: lookup("RDP_DOMAIN").filter(|domain| !domain.is_empty()),
        }))
    }
}

impl CredentialResolver for EnvCredentials {
    fn resolve<'a>(
        &'a self,
        _host: &'a str,
        _port: u16,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<RdpCredentials>> + Send + 'a>> {
        let credentials = self.0.clone();
        Box::pin(async move { Ok(credentials) })
    }
}
