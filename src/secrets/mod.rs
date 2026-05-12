//! Secret-backend strategy. Each backend implements [`SecretBackend`] and
//! resolves a single secret value at request time. Selection is config-driven.

use anyhow::Result;
use secrecy::SecretString;
use std::sync::Arc;

use crate::process::ProcessInvoker;

pub mod env;
pub mod keychain;
pub mod one_password;

/// Serialized form of a backend configuration. Discriminated by `backend`:
///
/// ```toml
/// [secrets.claude_code_oauth_token]
/// backend = "op"
/// ref = "op://Employee/Claude Code Auth Token/credential"
/// ```
///
/// Alternatives:
///
/// ```toml
/// backend = "env"
/// var = "CLAUDE_CODE_OAUTH_TOKEN"
/// ```
///
/// ```toml
/// backend = "keychain"
/// service = "claude-code-oauth-token"
/// # account = "niklas"   # defaults to $USER
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "backend", rename_all = "kebab-case")]
pub enum SecretBackendConfig {
    Env {
        var: String,
    },
    Keychain {
        service: String,
        #[serde(default)]
        account: Option<String>,
    },
    Op {
        #[serde(rename = "ref")]
        reference: String,
    },
}

impl SecretBackendConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Env { .. } => "env",
            Self::Keychain { .. } => "keychain",
            Self::Op { .. } => "op",
        }
    }
}

/// A backend that can produce a [`SecretString`].
pub trait SecretBackend {
    /// Stable identifier for the backend kind (`env` / `keychain` / `op`).
    fn kind(&self) -> &'static str;

    /// Resolve the secret. May block on a subprocess (keychain / op).
    fn fetch(&self) -> Result<SecretString>;
}

/// Construct a concrete backend from a config entry.
pub fn build(
    cfg: &SecretBackendConfig,
    invoker: Arc<dyn ProcessInvoker>,
) -> Box<dyn SecretBackend> {
    match cfg {
        SecretBackendConfig::Env { var } => Box::new(env::EnvBackend::new(var.clone())),
        SecretBackendConfig::Keychain { service, account } => Box::new(
            keychain::KeychainBackend::new(service.clone(), account.clone()),
        ),
        SecretBackendConfig::Op { reference } => {
            Box::new(one_password::OpBackend::new(reference.clone(), invoker))
        }
    }
}
