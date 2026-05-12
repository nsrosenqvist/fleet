//! 1Password CLI backend. Reads via `op read <reference>` where reference is
//! a secret reference like `op://Vault/Item/field`.

use anyhow::Result;
use secrecy::SecretString;
use std::sync::Arc;

use crate::process::ProcessInvoker;

pub struct OpBackend {
    reference: String,
    invoker: Arc<dyn ProcessInvoker>,
}

impl OpBackend {
    pub fn new(reference: String, invoker: Arc<dyn ProcessInvoker>) -> Self {
        Self { reference, invoker }
    }
}

impl super::SecretBackend for OpBackend {
    fn kind(&self) -> &'static str {
        "op"
    }

    fn fetch(&self) -> Result<SecretString> {
        let out = self
            .invoker
            .run("op", vec!["read".to_string(), self.reference.clone()])?;
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

    #[test]
    fn invokes_op_read_with_reference() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(
                eq("op"),
                eq(vec![
                    "read".to_string(),
                    "op://Vault/Item/credential".to_string(),
                ]),
            )
            .returning(|_, _| Ok(String::from("sk-ant-oat01-xxx")));
        let b = OpBackend::new("op://Vault/Item/credential".to_string(), Arc::new(mock));
        let s = b.fetch().expect("ok");
        assert_eq!(s.expose_secret(), "sk-ant-oat01-xxx");
    }
}
