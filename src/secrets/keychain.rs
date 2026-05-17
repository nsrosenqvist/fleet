//! OS-native keyring backend.
//!
//! Resolves via the `keyring` crate, which routes to:
//! - macOS: Security framework (same store the `security` CLI uses)
//! - Linux: DBus Secret Service (GNOME Keyring / KWallet / KeePassXC / …)
//! - Windows: Credential Manager
//!
//! Storage shape is `(service, account)`. Concrete entry creation:
//! - macOS: `security add-generic-password -a $USER -s <service> -w <secret>`
//! - Linux: `secret-tool store --label='fleet' service <service> account $USER`
//! - Windows: `cmdkey /generic:<service> /user:%USERNAME% /pass:<secret>`
//!
//! `fleet secrets register <name>` runs the appropriate command for
//! the user's platform so they don't have to remember the syntax.

use anyhow::{Context, Result};
use keyring::Entry;
use secrecy::SecretString;

pub struct KeychainBackend {
    service: String,
    account: Option<String>,
}

impl KeychainBackend {
    #[must_use]
    pub fn new(service: String, account: Option<String>) -> Self {
        Self { service, account }
    }

    /// Default the account to `$USER` so workstation use is
    /// configuration-free. Falls back to `"default"` when even that
    /// isn't set (sandboxed CI containers), keeping the entry name
    /// stable rather than failing on resolution.
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
        // `Entry::new` fails when the platform credential store is
        // entirely unreachable (e.g. headless Linux with no DBus
        // session bus and no fallback). Surface that as an
        // actionable message rather than the keyring crate's debug-
        // formatted platform-specific error.
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
                 Run `fleet secrets register {0}` to set it up, or store it \
                 directly with `secret-tool store --label='fleet' service {0} \
                 account {1}` (Linux) / `security add-generic-password -a {1} \
                 -s {0} -w '<token>'` (macOS).",
                self.service, account,
            )
        })?;
        Ok(SecretString::from(secret))
    }
}
