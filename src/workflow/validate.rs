//! Static DAG validation for [`Workflow`]s.
//!
//! Three checks today:
//!   1. Node ids are unique.
//!   2. Every `depends_on` / `loop_back_to` / `fanout.siblings` target
//!      names an existing node.
//!   3. The `depends_on` graph has no cycles.
//!
//! `loop_back_to` is intentionally *not* counted as a graph cycle — it's
//! a revision construct with a `max_loops` bound, and the executor handles
//! it explicitly. Cycle detection here is only about the forward DAG.
//!
//! Semantic validation (agent name resolves in the registry, network
//! allowlist covers tracker host, prompt files exist) is Phase 2 work and
//! lives in the executor or a separate `semantic.rs` later.

use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

use super::spec::{NodeKind, Workflow};

/// Run all static checks on `wf`. Returns the first error found; callers
/// surface it verbatim — error wording is part of the user-facing contract.
pub fn validate(wf: &Workflow) -> Result<()> {
    check_unique_ids(wf)?;
    check_references(wf)?;
    check_no_cycles(wf)?;
    Ok(())
}

fn check_unique_ids(wf: &Workflow) -> Result<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    for n in &wf.nodes {
        if !seen.insert(n.id.as_str()) {
            bail!("workflow `{}` has duplicate node id `{}`", wf.name, n.id);
        }
    }
    Ok(())
}

fn check_references(wf: &Workflow) -> Result<()> {
    let ids: HashSet<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
    let id_must_exist = |target: &str, context: &str, owner: &str| -> Result<()> {
        if !ids.contains(target) {
            bail!(
                "workflow `{}` node `{owner}` references unknown node `{target}` via {context}",
                wf.name
            );
        }
        Ok(())
    };
    for n in &wf.nodes {
        for dep in &n.depends_on {
            id_must_exist(dep, "depends_on", &n.id)?;
        }
        if let Some(lb) = &n.loop_back_to {
            id_must_exist(lb, "loop_back_to", &n.id)?;
        }
        if let NodeKind::Fanout { siblings } = &n.kind {
            for s in siblings {
                id_must_exist(s, "fanout.siblings", &n.id)?;
            }
        }
    }
    Ok(())
}

fn check_no_cycles(wf: &Workflow) -> Result<()> {
    // Build adjacency: each node id → its `depends_on` predecessors. For
    // cycle detection we walk it as a forward edge "n → dep" (n depends
    // on dep, so dep must run before n; a back-edge in DFS means a cycle).
    let mut adjacency: HashMap<&str, &[String]> = HashMap::new();
    for n in &wf.nodes {
        adjacency.insert(n.id.as_str(), &n.depends_on);
    }

    let mut state: HashMap<&str, Visit> = HashMap::new();
    for n in &wf.nodes {
        if !state.contains_key(n.id.as_str()) {
            if let Some(cycle_through) = dfs(n.id.as_str(), &adjacency, &mut state) {
                bail!(
                    "workflow `{}` has a dependency cycle through node `{cycle_through}`",
                    wf.name
                );
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    InProgress,
    Done,
}

/// Iterative DFS over `depends_on` edges. Returns the first node id we see
/// completing a back-edge so the error message can name it. `state` is a
/// shared progress map so multiple roots don't re-walk shared subgraphs.
fn dfs<'a>(
    start: &'a str,
    adjacency: &HashMap<&'a str, &'a [String]>,
    state: &mut HashMap<&'a str, Visit>,
) -> Option<&'a str> {
    // Use an explicit stack to avoid recursion + the call-frame growth on
    // very deep DAGs. Each entry is (node, index-into-its-deps); the
    // index lets us resume after a child returns.
    let mut stack: Vec<(&'a str, usize)> = vec![(start, 0)];
    state.insert(start, Visit::InProgress);

    while let Some((node, idx)) = stack.last().copied() {
        let deps = adjacency.get(node).copied().unwrap_or(&[]);
        if idx >= deps.len() {
            state.insert(node, Visit::Done);
            stack.pop();
            continue;
        }
        // Advance the parent's index so we resume after this child returns.
        if let Some(slot) = stack.last_mut() {
            slot.1 = idx + 1;
        }
        let dep: &str = deps[idx].as_str();
        // Resolve `dep` against the keys in `adjacency`. If absent, this
        // is a reference error (already caught by `check_references`); the
        // DFS just skips unknown nodes to avoid a panic in the test path
        // where `validate` was called on raw input.
        if let Some(key) = adjacency.keys().find(|k| **k == dep).copied() {
            match state.get(key).copied() {
                Some(Visit::Done) => {}
                Some(Visit::InProgress) => return Some(key),
                None => {
                    state.insert(key, Visit::InProgress);
                    stack.push((key, 0));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::spec::Workflow;

    fn parse(yaml: &str) -> Workflow {
        Workflow::from_str_at(yaml, "/x").unwrap()
    }

    #[test]
    fn accepts_a_linear_dag() {
        let wf = parse(
            "\
name: linear
nodes:
  - id: a
    agent: claude
  - id: b
    depends_on: [a]
    agent: claude
  - id: c
    depends_on: [b]
    agent: claude
",
        );
        validate(&wf).unwrap();
    }

    #[test]
    fn accepts_a_diamond_dag() {
        let wf = parse(
            "\
name: diamond
nodes:
  - id: a
    agent: claude
  - id: b1
    depends_on: [a]
    agent: claude
  - id: b2
    depends_on: [a]
    agent: claude
  - id: c
    depends_on: [b1, b2]
    agent: claude
",
        );
        validate(&wf).unwrap();
    }

    #[test]
    fn rejects_duplicate_node_ids() {
        let wf = parse(
            "\
name: dup
nodes:
  - id: a
    agent: claude
  - id: a
    agent: claude
",
        );
        let err = validate(&wf).unwrap_err();
        assert!(format!("{err}").contains("duplicate node id `a`"));
    }

    #[test]
    fn rejects_depends_on_unknown_node() {
        let wf = parse(
            "\
name: dangling
nodes:
  - id: b
    depends_on: [a]
    agent: claude
",
        );
        let err = validate(&wf).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("references unknown node `a`"));
        assert!(msg.contains("via depends_on"));
    }

    #[test]
    fn rejects_loop_back_to_unknown_node() {
        let wf = parse(
            "\
name: bad-loop
nodes:
  - id: a
    agent: claude
    loop_back_to: nowhere
",
        );
        let err = validate(&wf).unwrap_err();
        assert!(format!("{err}").contains("via loop_back_to"));
    }

    #[test]
    fn rejects_fanout_sibling_unknown_node() {
        let wf = parse(
            "\
name: bad-fan
nodes:
  - id: f
    type: fanout
    siblings: [a, b]
  - id: a
    agent: claude
",
        );
        let err = validate(&wf).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown node `b`"));
        assert!(msg.contains("fanout.siblings"));
    }

    #[test]
    fn rejects_two_node_cycle() {
        let wf = parse(
            "\
name: cycle
nodes:
  - id: a
    depends_on: [b]
    agent: claude
  - id: b
    depends_on: [a]
    agent: claude
",
        );
        let err = validate(&wf).unwrap_err();
        assert!(format!("{err}").contains("dependency cycle"));
    }

    #[test]
    fn rejects_self_dependency() {
        let wf = parse(
            "\
name: selfloop
nodes:
  - id: a
    depends_on: [a]
    agent: claude
",
        );
        let err = validate(&wf).unwrap_err();
        assert!(format!("{err}").contains("dependency cycle"));
    }

    #[test]
    fn rejects_three_node_cycle() {
        let wf = parse(
            "\
name: ring
nodes:
  - id: a
    depends_on: [c]
    agent: claude
  - id: b
    depends_on: [a]
    agent: claude
  - id: c
    depends_on: [b]
    agent: claude
",
        );
        let err = validate(&wf).unwrap_err();
        assert!(format!("{err}").contains("dependency cycle"));
    }

    #[test]
    fn loop_back_to_does_not_count_as_a_cycle() {
        // `revise → review` via loop_back_to is allowed even though the
        // forward DAG already has `revise depends_on review`.
        let wf = parse(
            "\
name: rev
nodes:
  - id: review
    agent: claude
  - id: revise
    depends_on: [review]
    agent: claude
    loop_back_to: review
    max_loops: 2
",
        );
        validate(&wf).unwrap();
    }

    #[test]
    fn empty_workflow_is_valid() {
        let wf = parse("name: empty\nnodes: []");
        validate(&wf).unwrap();
    }

}
