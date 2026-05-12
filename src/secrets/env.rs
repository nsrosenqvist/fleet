//! Environment-variable backend. Reads `$VAR` at fetch time. Most useful for
//! CI and explicit local overrides.

use anyhow::{Context, Result, bail};
use secrecy::SecretString;

pub struct EnvBackend {
    var: String,
}

impl EnvBackend {
    pub fn new(var: String) -> Self {
        Self { var }
    }
}

impl super::SecretBackend for EnvBackend {
    fn kind(&self) -> &'static str {
        "env"
    }

    fn fetch(&self) -> Result<SecretString> {
        let value = std::env::var(&self.var)
            .with_context(|| format!("env var `{}` is not set", self.var))?;
        if value.is_empty() {
            bail!("env var `{}` is set but empty", self.var);
        }
        Ok(SecretString::from(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretBackend as _;
    use secrecy::ExposeSecret;

    // Distinct names per test so concurrent tests can't race on the same env
    // var. Each name is unique to this module + test.
    const SET_VAR: &str = "FLEET_TEST_SECRET_ENV_BACKEND_SET";
    const UNSET_VAR: &str = "FLEET_TEST_SECRET_ENV_BACKEND_UNSET";

    #[test]
    fn fetches_when_set() {
        // SAFETY: env mutation is process-global; the name is unique to this
        // test so concurrent tests cannot race.
        unsafe {
            std::env::set_var(SET_VAR, "secret-value-123");
        }
        let backend = EnvBackend::new(SET_VAR.to_string());
        let s = backend.fetch().expect("fetch ok");
        assert_eq!(s.expose_secret(), "secret-value-123");
        unsafe {
            std::env::remove_var(SET_VAR);
        }
    }

    #[test]
    fn errors_when_unset() {
        // SAFETY: env mutation is process-global; the name is unique to this
        // test so concurrent tests cannot race.
        unsafe {
            std::env::remove_var(UNSET_VAR);
        }
        let backend = EnvBackend::new(UNSET_VAR.to_string());
        let err = backend.fetch().expect_err("should be unset");
        let msg = format!("{err:?}");
        assert!(msg.contains("not set"), "unexpected error: {msg}");
    }
}
