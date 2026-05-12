//! Cross-platform credential storage.
//!
//! Resolves a secret from the OS-native keyring via the `keyring` crate:
//! - macOS: Security framework (same data the `security` CLI sees).
//! - Linux: `DBus` Secret Service (`GNOME Keyring`, `KWallet`, `KeePassXC`, …).
//! - Windows: Credential Manager.
//!
//! Storage shape is `(service, account)`. macOS readers can populate the
//! same entry with:
//!     `security add-generic-password -a $USER -s <service> -w <secret>`
//! Linux readers can populate it via the `GNOME Keyring` UI, `secret-tool
//! store`, `KWallet`, or any other Secret Service client.

use anyhow::{Context, Result};
use keyring::Entry;
use secrecy::SecretString;

pub struct KeychainBackend {
    service: String,
    account: Option<String>,
}

impl KeychainBackend {
    pub fn new(service: String, account: Option<String>) -> Self {
        Self { service, account }
    }

    fn resolved_account(&self) -> String {
        self.account
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| String::from("default"))
    }
}

impl super::SecretBackend for KeychainBackend {
    fn kind(&self) -> &'static str {
        "keychain"
    }

    fn fetch(&self) -> Result<SecretString> {
        let account = self.resolved_account();
        // `Entry::new` only fails when the platform credential store is
        // entirely unreachable (e.g. headless Linux with no DBus session
        // bus and no fallback). Surface that as a clear actionable
        // message rather than the keyring crate's debug-formatted
        // platform-specific error.
        let entry = Entry::new(&self.service, &account).with_context(|| {
            format!(
                "open OS keyring for service `{}` account `{}` — \
                 is a Secret Service provider running (GNOME Keyring, KWallet, \
                 KeePassXC) and is `$DBUS_SESSION_BUS_ADDRESS` set?",
                self.service, account,
            )
        })?;
        let secret = entry.get_password().with_context(|| {
            format!(
                "no entry in OS keyring for service `{}` account `{}`. \
                 Store it with `secret-tool store --label='fleet' \
                 service {0} account {1}` on Linux or \
                 `security add-generic-password -a {1} -s {0} -w '<token>'` on macOS.",
                self.service, account,
            )
        })?;
        Ok(SecretString::from(secret))
    }
}
