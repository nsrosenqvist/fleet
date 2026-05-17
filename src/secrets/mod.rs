//! Secret-backend strategy. Each backend implements [`SecretBackend`]
//! and resolves a single secret value at request time. Selection is
//! config-driven: users declare a `secrets:` block in
//! `.fleet/config.yaml`, fleet builds the matching backend, and the
//! agent-env layer calls `fetch()` when a workflow needs the value.
//!
//! Pre-AO/Lima fleet had the same shape; the module survived the
//! refactor essentially unchanged because the abstraction is between
//! "config declares a backend" and "agent env needs a token" — both
//! sides still exist now, just with a container at the receiving
//! end instead of a VM.
//!
//! Three backends today:
//! - [`env::EnvBackend`] — read `$VAR` from the host process env.
//!   Cheapest, fits CI workflows where the token is already an env
//!   var of the runner.
//! - [`keychain::KeychainBackend`] — OS-native keyring (macOS
//!   Security framework, Linux DBus Secret Service, Windows
//!   Credential Manager). The recommended path for interactive
//!   workstations; `fleet secrets register` walks the user through
//!   storing the token here.
//! - [`one_password::OpBackend`] — shells out to `op read
//!   op://Vault/Item/field`. Common in shops that already mandate
//!   1Password.

use anyhow::Result;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::process::ProcessInvoker;

pub mod env;
pub mod keychain;
pub mod one_password;

/// Serialised form of a backend configuration. Discriminated by
/// `backend:` — clap's yaml/serde derive matches whichever tag is set.
///
/// ```yaml
/// secrets:
///   claude_code_oauth_token:
///     backend: keychain
///     service: claude-code-oauth-token
///     # account defaults to $USER
///
///   gh_token:
///     backend: env
///     var: GH_TOKEN
///
///   sentry_dsn:
///     backend: op
///     ref: op://Eng/Sentry/dsn
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "backend", rename_all = "kebab-case")]
pub enum SecretBackendConfig {
    /// Read from a host env var.
    Env { var: String },
    /// Read from the OS keyring.
    Keychain {
        service: String,
        #[serde(default)]
        account: Option<String>,
    },
    /// Shell `op read <reference>` (1Password CLI).
    Op {
        #[serde(rename = "ref")]
        reference: String,
    },
}

impl SecretBackendConfig {
    /// Short kind name for diagnostics + the `fleet secrets list`
    /// output. Stable wording — tests pin it.
    #[must_use]
    #[allow(dead_code)] // surfaced via tests + future CLI uses
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Env { .. } => "env",
            Self::Keychain { .. } => "keychain",
            Self::Op { .. } => "op",
        }
    }

    /// One-line summary suitable for the `list` CLI. Doesn't leak
    /// the secret value — only the reference shape.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Env { var } => format!("env {var}"),
            Self::Keychain { service, account } => account.as_ref().map_or_else(
                || format!("keychain {service}"),
                |a| format!("keychain {service} (account: {a})"),
            ),
            Self::Op { reference } => format!("op {reference}"),
        }
    }
}

/// A backend that can produce a [`SecretString`]. Implementations
/// own the configuration they need to do the lookup; the trait stays
/// minimal so tests can mock it without dragging in real keyrings or
/// `op` binaries.
pub trait SecretBackend: Send + Sync {
    /// Stable identifier for the backend kind. Same string as
    /// [`SecretBackendConfig::kind`] for the matching variant.
    fn kind(&self) -> &'static str;

    /// Resolve the secret. May block on a subprocess (keychain / op).
    /// Errors carry actionable context — the message is what the user
    /// sees when `fleet secrets test <name>` fails.
    fn fetch(&self) -> Result<SecretString>;
}

/// Construct a concrete backend from a config entry. The `invoker`
/// parameter is the seam through which the `op` backend shells out;
/// `env` and `keychain` ignore it.
#[must_use]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_kind_matches_serde_tag() {
        // The kind string is what `fleet secrets list` prints; pin
        // it against the variant so a rename can't drift the
        // user-facing wording.
        assert_eq!(
            SecretBackendConfig::Env {
                var: "X".to_string()
            }
            .kind(),
            "env"
        );
        assert_eq!(
            SecretBackendConfig::Keychain {
                service: "s".to_string(),
                account: None
            }
            .kind(),
            "keychain"
        );
        assert_eq!(
            SecretBackendConfig::Op {
                reference: "r".to_string()
            }
            .kind(),
            "op"
        );
    }

    #[test]
    fn describe_does_not_include_a_value() {
        // `describe` shows up in the list CLI. Sanity: it carries
        // only the *reference* shape, never anything that could
        // contain the resolved token.
        let c = SecretBackendConfig::Env {
            var: "GH_TOKEN".to_string(),
        };
        let d = c.describe();
        assert!(d.contains("env"));
        assert!(d.contains("GH_TOKEN"));
    }

    #[test]
    fn config_deserialises_keychain_with_default_account() {
        let yaml = "backend: keychain\nservice: claude-code-oauth-token\n";
        let c: SecretBackendConfig = serde_yml::from_str(yaml).unwrap();
        match c {
            SecretBackendConfig::Keychain { service, account } => {
                assert_eq!(service, "claude-code-oauth-token");
                assert!(account.is_none(), "default account should be missing");
            }
            other => panic!("expected keychain, got {other:?}"),
        }
    }

    #[test]
    fn config_deserialises_op_via_aliased_ref_field() {
        // `ref` is a reserved word in Rust; serde aliases the field
        // so YAML can use the natural spelling.
        let yaml = "backend: op\nref: op://Vault/Item/credential\n";
        let c: SecretBackendConfig = serde_yml::from_str(yaml).unwrap();
        match c {
            SecretBackendConfig::Op { reference } => {
                assert_eq!(reference, "op://Vault/Item/credential");
            }
            other => panic!("expected op, got {other:?}"),
        }
    }

    #[test]
    fn config_deserialises_env_simple() {
        let yaml = "backend: env\nvar: GH_TOKEN\n";
        let c: SecretBackendConfig = serde_yml::from_str(yaml).unwrap();
        match c {
            SecretBackendConfig::Env { var } => assert_eq!(var, "GH_TOKEN"),
            other => panic!("expected env, got {other:?}"),
        }
    }
}
