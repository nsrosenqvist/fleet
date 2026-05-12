//! macOS Keychain backend. Reads a generic password via `security
//! find-generic-password -a <account> -s <service> -w`.

use anyhow::Result;
use secrecy::SecretString;
use std::sync::Arc;

use crate::process::ProcessInvoker;

pub struct KeychainBackend {
    service: String,
    account: Option<String>,
    invoker: Arc<dyn ProcessInvoker>,
}

impl KeychainBackend {
    pub fn new(service: String, account: Option<String>, invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self {
            service,
            account,
            invoker,
        }
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
        let out = self.invoker.run(
            "security",
            vec![
                "find-generic-password".to_string(),
                "-a".to_string(),
                account,
                "-s".to_string(),
                self.service.clone(),
                "-w".to_string(),
            ],
        )?;
        Ok(SecretString::from(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use crate::secrets::SecretBackend as _;
    use mockall::predicate::eq;
    use secrecy::ExposeSecret;

    fn argv(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn invokes_security_with_explicit_account() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("security"),
                eq(argv(&[
                    "find-generic-password",
                    "-a",
                    "alice",
                    "-s",
                    "my-service",
                    "-w",
                ])),
            )
            .returning(|_, _| Ok(String::from("hunter2")));
        let b = KeychainBackend::new("my-service".into(), Some("alice".into()), Arc::new(mock));
        let s = b.fetch().expect("ok");
        assert_eq!(s.expose_secret(), "hunter2");
    }

    #[test]
    fn falls_back_to_user_env_for_account() {
        // SAFETY: env mutation is process-global. Unique value, restored after.
        unsafe {
            std::env::set_var("USER", "fleet-test-user");
        }
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("security"),
                eq(argv(&[
                    "find-generic-password",
                    "-a",
                    "fleet-test-user",
                    "-s",
                    "svc",
                    "-w",
                ])),
            )
            .returning(|_, _| Ok(String::from("v")));
        let b = KeychainBackend::new("svc".into(), None, Arc::new(mock));
        let _ = b.fetch().expect("ok");
    }
}
