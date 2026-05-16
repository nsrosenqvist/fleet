//! Outcome convention: a per-node-output protocol that lets agents
//! signal "I finished" / "I'm blocked" / "I made progress" in a
//! structured way the workflow engine can branch on. The protocol is
//! optional — nodes that don't declare an `outcome` output stay on the
//! pre-convention "succeeded / failed" semantics.
//!
//! Why a free-standing module rather than baking it into
//! [`extract_outputs`](super::executor::extract_outputs): the
//! convention is a *layer* on top of the generic output-extraction
//! mechanism. Keeping it separate means non-orchestrator workflows
//! never see the validation, and the rules stay readable in one place.
//!
//! Today the validation runs at extraction time. The workflow
//! executor calls [`validate_outcome`] after [`extract_outputs`] has
//! landed the flat `(node, name) -> String` entries; a bad outcome
//! fails the node like a missing artifact would.

use anyhow::{Result, bail};

use super::expr::OutputMap;
use super::spec::Node;

/// Name of the agent output that carries the outcome. Workflow YAML
/// declares it as `outputs: { outcome: <source-key-in-result-json> }`.
pub const OUTCOME_OUTPUT_KEY: &str = "outcome";

/// Companion output carrying a free-text reason when the agent reports
/// `blocked`. Optional: workflows that don't need machine-readable
/// blockers can omit it from their `outputs:` map.
pub const BLOCKER_OUTPUT_KEY: &str = "blocker";

/// Outcome strings the agent is allowed to write into
/// `result.outcome`. These are the only three the workflow engine
/// branches on; anything else is rejected at extraction time so a
/// silently-mistyped outcome (e.g. `"blockd"`) doesn't route a
/// workflow into the wrong handler.
pub const OUTCOME_PROGRESS: &str = "progress";
pub const OUTCOME_BLOCKED: &str = "blocked";
pub const OUTCOME_DONE: &str = "done";

/// Validate the agent's declared outcome against the convention.
///
/// Contract:
/// - When the node does **not** declare an `outcome` output, this is a
///   no-op. The convention is opt-in.
/// - When `outcome` is declared, the extracted value must be one of
///   `progress` / `blocked` / `done`.
/// - When `outcome == "blocked"` AND the node also declares a
///   `blocker` output, the blocker must be present and non-empty.
///   Workflows that omit `blocker` aren't held to this — they've
///   chosen not to capture the reason.
///
/// `extract_outputs` is expected to have already run and populated
/// `outputs` for this node before this is called. A declared-but-not-
/// extracted outcome is an internal invariant violation and surfaces
/// as an error so the caller doesn't proceed past a half-populated
/// state.
pub fn validate_outcome(node: &Node, outputs: &OutputMap) -> Result<()> {
    if !node.outputs.contains_key(OUTCOME_OUTPUT_KEY) {
        // Opt-in protocol: no `outcome:` declared means no validation.
        return Ok(());
    }
    let outcome = outputs
        .get(&(node.id.clone(), OUTCOME_OUTPUT_KEY.to_string()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "node `{}`: declared an `outcome` output but the extractor \
                 did not populate it (this is an internal invariant violation; \
                 extract_outputs should have errored first)",
                node.id,
            )
        })?;
    match outcome.as_str() {
        OUTCOME_PROGRESS | OUTCOME_DONE => Ok(()),
        OUTCOME_BLOCKED => {
            // Only require `blocker` if the workflow opted into it.
            if !node.outputs.contains_key(BLOCKER_OUTPUT_KEY) {
                return Ok(());
            }
            let blocker = outputs
                .get(&(node.id.clone(), BLOCKER_OUTPUT_KEY.to_string()))
                .map_or("", String::as_str);
            if blocker.trim().is_empty() {
                bail!(
                    "node `{}`: outcome is `blocked` but the declared `blocker` \
                     output is empty — the agent must explain what is blocking it \
                     so a follow-up workflow node (or human) can act on the reason",
                    node.id
                );
            }
            Ok(())
        }
        other => bail!(
            "node `{}`: outcome must be one of `{OUTCOME_PROGRESS}`, \
             `{OUTCOME_BLOCKED}`, or `{OUTCOME_DONE}`; got `{other}`",
            node.id
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::spec::{ArtifactsSpec, Node, NodeKind};
    use std::collections::BTreeMap;

    /// Build a minimal agent node with the given (local -> source) outputs
    /// map. The other fields are placeholders; the validator doesn't
    /// look at them.
    fn node_with_outputs(id: &str, outputs: &[(&str, &str)]) -> Node {
        let mut decl = BTreeMap::new();
        for (local, source) in outputs {
            decl.insert((*local).to_string(), (*source).to_string());
        }
        Node {
            id: id.to_string(),
            depends_on: Vec::new(),
            when: None,
            kind: NodeKind::Agent {
                agent: "x".to_string(),
                persona: None,
                prompt_file: None,
            },
            artifacts: ArtifactsSpec {
                r#in: Vec::new(),
                out: Vec::new(),
            },
            outputs: decl,
            loop_back_to: None,
            max_loops: None,
        }
    }

    fn outputs_with(entries: &[(&str, &str, &str)]) -> OutputMap {
        entries
            .iter()
            .map(|(node, name, val)| {
                (
                    ((*node).to_string(), (*name).to_string()),
                    (*val).to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn validate_outcome_is_a_noop_when_node_does_not_declare_outcome() {
        // Backwards-compat: workflows from before the convention still
        // pass through untouched.
        let n = node_with_outputs("plan", &[("decision", "decision")]);
        let outputs = outputs_with(&[("plan", "decision", "approve")]);
        validate_outcome(&n, &outputs).unwrap();
    }

    #[test]
    fn validate_outcome_accepts_progress() {
        let n = node_with_outputs("implement", &[("outcome", "outcome")]);
        let outputs = outputs_with(&[("implement", "outcome", "progress")]);
        validate_outcome(&n, &outputs).unwrap();
    }

    #[test]
    fn validate_outcome_accepts_done() {
        let n = node_with_outputs("implement", &[("outcome", "outcome")]);
        let outputs = outputs_with(&[("implement", "outcome", "done")]);
        validate_outcome(&n, &outputs).unwrap();
    }

    #[test]
    fn validate_outcome_accepts_blocked_without_blocker_when_workflow_did_not_declare_one() {
        // If the workflow author didn't ask for a blocker, the agent
        // isn't held to producing one. The opt-in nature of the field
        // matters because not every blocked-handling workflow needs a
        // machine-readable reason.
        let n = node_with_outputs("implement", &[("outcome", "outcome")]);
        let outputs = outputs_with(&[("implement", "outcome", "blocked")]);
        validate_outcome(&n, &outputs).unwrap();
    }

    #[test]
    fn validate_outcome_accepts_blocked_with_non_empty_blocker() {
        let n = node_with_outputs(
            "implement",
            &[("outcome", "outcome"), ("blocker", "blocker")],
        );
        let outputs = outputs_with(&[
            ("implement", "outcome", "blocked"),
            ("implement", "blocker", "needs maintainer review on PR #42"),
        ]);
        validate_outcome(&n, &outputs).unwrap();
    }

    #[test]
    fn validate_outcome_rejects_blocked_with_empty_blocker_when_declared() {
        let n = node_with_outputs(
            "implement",
            &[("outcome", "outcome"), ("blocker", "blocker")],
        );
        let outputs = outputs_with(&[
            ("implement", "outcome", "blocked"),
            ("implement", "blocker", ""),
        ]);
        let err = validate_outcome(&n, &outputs).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("blocker") && msg.contains("empty"),
            "got: {msg}"
        );
    }

    #[test]
    fn validate_outcome_rejects_blocked_with_whitespace_only_blocker() {
        // Trim before length-check so an agent that "filled in"
        // the blocker with a couple of spaces still fails — the goal
        // is a machine-readable reason, not the appearance of one.
        let n = node_with_outputs(
            "implement",
            &[("outcome", "outcome"), ("blocker", "blocker")],
        );
        let outputs = outputs_with(&[
            ("implement", "outcome", "blocked"),
            ("implement", "blocker", "   \t\n"),
        ]);
        let err = validate_outcome(&n, &outputs).unwrap_err();
        assert!(format!("{err}").contains("empty"));
    }

    #[test]
    fn validate_outcome_rejects_unknown_value() {
        let n = node_with_outputs("implement", &[("outcome", "outcome")]);
        let outputs = outputs_with(&[("implement", "outcome", "blockd")]);
        let err = validate_outcome(&n, &outputs).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("blockd"), "got: {msg}");
        assert!(msg.contains("progress"), "got: {msg}");
        assert!(msg.contains("blocked"), "got: {msg}");
        assert!(msg.contains("done"), "got: {msg}");
    }

    #[test]
    fn validate_outcome_is_case_sensitive() {
        // "Done" with a capital D is not accepted — the spec is
        // explicit about the three lowercase strings; case-insensitive
        // matching would silently mask typos in the prompt.
        let n = node_with_outputs("implement", &[("outcome", "outcome")]);
        let outputs = outputs_with(&[("implement", "outcome", "Done")]);
        let err = validate_outcome(&n, &outputs).unwrap_err();
        assert!(format!("{err}").contains("Done"));
    }

    #[test]
    fn validate_outcome_errors_when_declared_but_not_extracted() {
        // Internal invariant. If a future caller forgets to run
        // extract_outputs first, this is the surface that catches it.
        let n = node_with_outputs("implement", &[("outcome", "outcome")]);
        let outputs = OutputMap::new();
        let err = validate_outcome(&n, &outputs).unwrap_err();
        assert!(format!("{err}").contains("did not populate"));
    }
}
