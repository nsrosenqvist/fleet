//! Doctor-view state + key handler. The snapshot is built lazily on
//! first entry to the view (and re-built on every subsequent `d`/`r`).

use crossterm::event::{KeyCode, KeyEvent};
use std::path::PathBuf;
use std::sync::Arc;

use crate::process::{ProcessInvoker, RealProcessInvoker};
use crate::repo;
use crate::repo_config::RepoConfig;
use crate::runtime::Capabilities;
use crate::runtime::detect::probe;
use crate::runtime::factory::build_adapter;
use crate::session::store::SessionStore;

use super::{Action, AppState, View};

/// Resolved snapshot for the doctor pane.
#[derive(Debug, Clone)]
pub struct DoctorSnapshot {
    pub root: PathBuf,
    pub initialised: bool,
    pub configured_adapter: String,
    pub configured_hardening: String,
    pub tracker: String,
    pub adapter: Result<(String, Capabilities), String>,
    pub agents: Vec<(String, Vec<String>)>,
}

impl DoctorSnapshot {
    pub fn probe(root: PathBuf) -> Self {
        let initialised = repo::is_initialised(&root);
        let config = RepoConfig::load(root.join(".fleet/config.yaml")).unwrap_or_default();
        let invoker: Arc<dyn ProcessInvoker> = Arc::new(RealProcessInvoker);
        let probe_report = probe(invoker.as_ref());
        let adapter = match build_adapter(&config.runtime, &probe_report, Arc::clone(&invoker)) {
            Ok(adapter) => Ok((adapter.name().to_string(), adapter.capabilities())),
            Err(err) => Err(format!("{err:#}")),
        };
        let agents: Vec<(String, Vec<String>)> = config
            .agents
            .registry
            .iter()
            .map(|(name, spec)| (name.to_string(), spec.env_passthrough.clone()))
            .collect();
        Self {
            root,
            initialised,
            configured_adapter: config.runtime.adapter.as_str().to_string(),
            configured_hardening: config.runtime.hardening.as_str().to_string(),
            tracker: config.tracker.as_str().to_string(),
            adapter,
            agents,
        }
    }
}

impl AppState {
    pub(in crate::tui) fn handle_key_doctor(
        &mut self,
        key: KeyEvent,
        store: &SessionStore,
    ) -> Action {
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc | KeyCode::Char('d') => {
                self.view = View::Sessions;
                Action::None
            }
            KeyCode::Char('r') => {
                if let Err(err) = self.reload(store) {
                    self.status_line = format!(" reload failed: {err:#} ");
                }
                self.doctor = Some(DoctorSnapshot::probe(self.root.clone()));
                Action::None
            }
            _ => Action::None,
        }
    }
}
