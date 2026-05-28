//! Per-session workflow progress — the data behind the Sessions
//! view's "what part of the workflow has run?" pane.
//!
//! Loads the session's workflow YAML, walks the DAG depth-first to
//! produce a flat list of [`ProgressRow`]s (one per node + per loop
//! edge) with rendering metadata (depth, branch glyphs, status) and
//! per-node status derived from the session's outputs / cost map +
//! per-node log files.
//!
//! Pure with respect to the inputs: tests construct a `Workflow` +
//! `Session` directly and assert on the row sequence without touching
//! disk. The TUI's `AppState` re-runs [`WorkflowProgress::build`] on
//! selection change and on auto-reload.
//!
//! Stays in `crate::tui::app` because the consumer is the sessions
//! detail pane; nothing outside the TUI looks at this.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::session::{Session, SessionState};
use crate::workflow::spec::{Node, NodeKind, Workflow};

/// Computed-once-per-render view of a session's workflow. Cached on
/// [`super::AppState`] and rebuilt when the selected session changes
/// or its meta.json is reloaded.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowProgress {
    /// Workflow name the session ran (`session.workflow`).
    pub workflow_name: String,
    /// Whether the workflow YAML was reachable + parseable. When
    /// `Err`, [`Self::rows`] is empty and the renderer shows a muted
    /// "workflow yaml missing" placeholder.
    pub load_error: Option<String>,
    /// Rendered top-to-bottom tree of nodes + loop-edge annotations.
    pub rows: Vec<ProgressRow>,
}

/// One visible row in the workflow progress pane.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressRow {
    pub node_id: String,
    pub kind_label: &'static str,
    pub status: NodeStatus,
    pub cost_usd: Option<f64>,
    /// Indentation in tree depth (root = 0). Renderer multiplies by
    /// a fixed indent width to produce leading spaces.
    pub depth: usize,
    /// Which tree-glyph to draw before the node id at this depth.
    pub branch: BranchGlyph,
    /// Verbatim `when:` expression when the node was gated; rendered
    /// as a muted trailing annotation (`(when: …)`). `None` when the
    /// node ran unconditionally.
    pub when_expr: Option<String>,
    /// Loop-back target when this row represents a `loop_back_to:`
    /// edge (`↺ <target>`). Mutually exclusive with the regular
    /// per-node fields above (status/kind etc. are placeholders here).
    pub loop_back: Option<String>,
}

/// Per-node execution state inferred from session + log dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// Node finished successfully (outputs present or skipped-log absent
    /// and the session moved past it).
    Done,
    /// `session.current_node == Some(node.id)` and session state is
    /// `Running` or `AwaitingGate`.
    Running,
    /// `session.current_node == Some(node.id)` and session state is
    /// `Failed` or `Crashed`.
    Failed,
    /// Node was reached but its `when:` predicate evaluated false.
    /// Detected via a `--- skipped:` line at the head of its log
    /// file.
    SkippedByGate,
    /// Node hasn't run yet — either the session is still progressing
    /// or it terminated before reaching this node.
    Pending,
}

/// Tree-line glyph drawn before the node id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchGlyph {
    /// Root node (depth 0) or no glyph needed.
    None,
    /// `├─▶` — sibling with more after it.
    Mid,
    /// `└─▶` — last sibling under a parent.
    Last,
}

impl WorkflowProgress {
    /// Build a progress view for a session. `session_dir` is
    /// `.fleet/sessions/<id>/`; used to scan per-node log files to
    /// distinguish `SkippedByGate` from `Pending`. `workflows_root`
    /// is `<repo>/.fleet/workflows/`.
    pub fn build(session: &Session, session_dir: &Path, workflows_root: &Path) -> Self {
        let workflow_name = session.workflow.clone();
        let wf_path = workflow_path(workflows_root, &workflow_name);
        let wf = match load_workflow(&wf_path) {
            Ok(wf) => wf,
            Err(err) => {
                return Self {
                    workflow_name,
                    load_error: Some(err),
                    rows: Vec::new(),
                };
            }
        };

        let status_map = derive_status_map(&wf, session, &session_dir.join("logs"));
        let rows = layout_rows(&wf, &status_map, &session.node_costs);

        Self {
            workflow_name,
            load_error: None,
            rows,
        }
    }
}

fn workflow_path(workflows_root: &Path, name: &str) -> PathBuf {
    workflows_root.join(format!("{name}.yaml"))
}

fn load_workflow(path: &Path) -> Result<Workflow, String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Workflow::from_str_at(&body, path).map_err(|e| format!("{e:#}"))
}

/// Compute per-node status from session state + log dir scan.
fn derive_status_map(
    wf: &Workflow,
    session: &Session,
    logs_dir: &Path,
) -> HashMap<String, NodeStatus> {
    let mut out: HashMap<String, NodeStatus> = HashMap::new();
    let current = session.current_node.as_deref();
    let terminal = session.state.is_terminal();
    let failed = matches!(session.state, SessionState::Failed | SessionState::Crashed);

    // Pass 1: pin current_node + completed (cost recorded ≡ ran).
    for node in &wf.nodes {
        let status = if Some(node.id.as_str()) == current {
            if failed { NodeStatus::Failed } else { NodeStatus::Running }
        } else if session.node_costs.contains_key(&node.id) {
            NodeStatus::Done
        } else if log_starts_with_skipped(logs_dir, &node.id) {
            NodeStatus::SkippedByGate
        } else if logs_dir.join(format!("{}.log", node.id)).exists() {
            // Log written but no cost (e.g. bash node, create-pr) →
            // treat as Done. The skipped variant is checked first so
            // a skip header doesn't fall through here.
            NodeStatus::Done
        } else if terminal {
            // Workflow has ended without reaching this node — render
            // as gate-skipped so the user understands it was bypassed
            // (not just "still pending"). Crude but correct in the
            // common case: a terminal-state DAG without a log file
            // means the branch wasn't taken.
            NodeStatus::SkippedByGate
        } else {
            NodeStatus::Pending
        };
        out.insert(node.id.clone(), status);
    }
    out
}

/// True when the per-node log file's first line is the executor's
/// "skipped because when:" marker.
fn log_starts_with_skipped(logs_dir: &Path, node_id: &str) -> bool {
    let path = logs_dir.join(format!("{node_id}.log"));
    let Ok(body) = std::fs::read_to_string(&path) else {
        return false;
    };
    body.starts_with("--- skipped:")
}

/// Build the depth-first row list. Visits each root, then each
/// child; back edges (`loop_back_to`) emit a separate annotation
/// row immediately after the source node — they aren't traversed,
/// avoiding infinite recursion.
fn layout_rows(
    wf: &Workflow,
    status: &HashMap<String, NodeStatus>,
    costs: &BTreeMap<String, f64>,
) -> Vec<ProgressRow> {
    // Build child adjacency: parent_id → [child_id] in YAML order
    // so the tree's left-to-right traversal matches the file.
    let mut children: HashMap<&str, Vec<&Node>> = HashMap::new();
    let mut indegree: HashMap<&str, usize> = HashMap::new();
    for n in &wf.nodes {
        indegree.entry(n.id.as_str()).or_insert(0);
        for parent in &n.depends_on {
            children
                .entry(parent.as_str())
                .or_default()
                .push(n);
            *indegree.entry(n.id.as_str()).or_insert(0) += 1;
        }
    }

    // Roots: indegree 0. Visit in YAML order.
    let roots: Vec<&Node> = wf
        .nodes
        .iter()
        .filter(|n| indegree.get(n.id.as_str()).copied().unwrap_or(0) == 0)
        .collect();

    let mut rows = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, root) in roots.iter().enumerate() {
        let last = i + 1 == roots.len();
        visit_node(
            root, 0, last, &children, status, costs, &mut visited, &mut rows,
        );
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn visit_node(
    node: &Node,
    depth: usize,
    is_last_sibling: bool,
    children: &HashMap<&str, Vec<&Node>>,
    status: &HashMap<String, NodeStatus>,
    costs: &BTreeMap<String, f64>,
    visited: &mut std::collections::HashSet<String>,
    out: &mut Vec<ProgressRow>,
) {
    if !visited.insert(node.id.clone()) {
        // Diamond pattern — node has multiple parents. Render the
        // first occurrence; later parents leave a "→ <id>" stub row
        // so the user still sees the join exists.
        out.push(ProgressRow {
            node_id: node.id.clone(),
            kind_label: "join",
            status: status
                .get(&node.id)
                .copied()
                .unwrap_or(NodeStatus::Pending),
            cost_usd: None,
            depth,
            branch: branch_for(depth, is_last_sibling),
            when_expr: None,
            loop_back: Some(node.id.clone()),
        });
        return;
    }
    out.push(ProgressRow {
        node_id: node.id.clone(),
        kind_label: kind_label(&node.kind),
        status: status
            .get(&node.id)
            .copied()
            .unwrap_or(NodeStatus::Pending),
        cost_usd: costs.get(&node.id).copied(),
        depth,
        branch: branch_for(depth, is_last_sibling),
        when_expr: node.when.clone(),
        loop_back: None,
    });

    // Loop-back edge annotation lives one level deeper than the node
    // itself so it visually hangs off it.
    if let Some(target) = node.loop_back_to.clone() {
        out.push(ProgressRow {
            node_id: node.id.clone(),
            kind_label: "loop",
            status: NodeStatus::Pending,
            cost_usd: None,
            depth: depth + 1,
            branch: BranchGlyph::Last,
            when_expr: None,
            loop_back: Some(target),
        });
    }

    let kids = children
        .get(node.id.as_str())
        .cloned()
        .unwrap_or_default();
    for (i, child) in kids.iter().enumerate() {
        let last = i + 1 == kids.len();
        visit_node(child, depth + 1, last, children, status, costs, visited, out);
    }
}

const fn branch_for(depth: usize, is_last_sibling: bool) -> BranchGlyph {
    if depth == 0 {
        BranchGlyph::None
    } else if is_last_sibling {
        BranchGlyph::Last
    } else {
        BranchGlyph::Mid
    }
}

const fn kind_label(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Agent { .. } => "agent",
        NodeKind::Bash { .. } => "bash",
        NodeKind::Gate { .. } => "gate",
        NodeKind::Assert { .. } => "assert",
        NodeKind::Fanout { .. } => "fanout",
        NodeKind::PrList { .. } => "pr-list",
        NodeKind::PrChecks { .. } => "pr-checks",
        NodeKind::CreatePr { .. } => "create-pr",
        NodeKind::PrComment { .. } => "pr-comment",
        NodeKind::TrackerCreate { .. } => "tracker-create",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionId, SessionState};
    use crate::workflow::spec::Workflow;

    const STANDARD_YAML: &str = r#"
name: standard
trigger: { manual: true }
nodes:
  - id: plan
    agent: claude-code
    artifacts:
      out: [plan.md]
  - id: implement
    depends_on: [plan]
    agent: claude-code
    artifacts:
      in: [plan.md]
      out: [implement.outputs.json]
    outputs:
      outcome: implement.outcome
  - id: review
    depends_on: [implement]
    agent: claude-code
    artifacts:
      out: [review.outputs.json]
    outputs:
      decision: review.decision
  - id: revise
    depends_on: [review]
    when: 'review.decision == "changes_requested"'
    agent: claude-code
    loop_back_to: review
    max_loops: 2
  - id: open_pr
    depends_on: [review]
    when: 'review.decision == "approve"'
    type: create-pr
    title: x
    body: y
"#;

    fn parse_standard() -> Workflow {
        Workflow::from_str_at(STANDARD_YAML, "/x.yaml").unwrap()
    }

    fn session_with(state: SessionState, current: Option<&str>) -> Session {
        let mut s = Session::new(SessionId::new("s-1"), "standard", 1);
        s.transition_to(SessionState::Running, 2).unwrap();
        if state != SessionState::Running {
            s.transition_to(state, 3).unwrap();
        }
        s.current_node = current.map(str::to_string);
        s
    }

    #[test]
    fn layout_lists_nodes_depth_first_with_fanout_glyphs() {
        let wf = parse_standard();
        let session = session_with(SessionState::Running, Some("plan"));
        let logs = tempfile::tempdir().unwrap();
        let status = derive_status_map(&wf, &session, logs.path());
        let costs = BTreeMap::new();
        let rows = layout_rows(&wf, &status, &costs);

        // plan → implement → review → (revise + loop-back) → open_pr
        let ids: Vec<&str> = rows.iter().map(|r| r.node_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["plan", "implement", "review", "revise", "revise", "open_pr"],
        );

        // The two `review` children (revise + open_pr) sit at depth 3
        // — `revise` is Mid (more siblings), `open_pr` is Last.
        let revise = rows.iter().find(|r| r.node_id == "revise" && r.loop_back.is_none()).unwrap();
        assert_eq!(revise.depth, 3);
        assert_eq!(revise.branch, BranchGlyph::Mid);
        let open_pr = rows.iter().find(|r| r.node_id == "open_pr").unwrap();
        assert_eq!(open_pr.depth, 3);
        assert_eq!(open_pr.branch, BranchGlyph::Last);
    }

    #[test]
    fn layout_emits_loop_back_annotation_row_after_revise() {
        let wf = parse_standard();
        let session = session_with(SessionState::Running, None);
        let logs = tempfile::tempdir().unwrap();
        let status = derive_status_map(&wf, &session, logs.path());
        let rows = layout_rows(&wf, &status, &BTreeMap::new());

        // Find the revise node and the loop annotation that follows.
        let revise_idx = rows
            .iter()
            .position(|r| r.node_id == "revise" && r.loop_back.is_none())
            .unwrap();
        let loop_row = &rows[revise_idx + 1];
        assert_eq!(loop_row.loop_back.as_deref(), Some("review"));
        assert_eq!(loop_row.kind_label, "loop");
        assert_eq!(loop_row.depth, 4); // one deeper than the revise row.
    }

    #[test]
    fn status_marks_current_node_running_and_others_pending() {
        let wf = parse_standard();
        let session = session_with(SessionState::Running, Some("implement"));
        let logs = tempfile::tempdir().unwrap();
        let map = derive_status_map(&wf, &session, logs.path());
        assert_eq!(map["implement"], NodeStatus::Running);
        assert_eq!(map["plan"], NodeStatus::Pending);
    }

    #[test]
    fn status_marks_node_done_when_cost_recorded() {
        let wf = parse_standard();
        let mut session = session_with(SessionState::Running, Some("implement"));
        session.record_node_cost("plan".to_string(), 0.12, 99);
        let logs = tempfile::tempdir().unwrap();
        let map = derive_status_map(&wf, &session, logs.path());
        assert_eq!(map["plan"], NodeStatus::Done);
    }

    #[test]
    fn status_marks_failed_for_current_node_when_session_failed() {
        let wf = parse_standard();
        let mut session = session_with(SessionState::Running, Some("review"));
        let _ = session.transition_to(SessionState::Failed, 99);
        session.current_node = Some("review".to_string());
        let logs = tempfile::tempdir().unwrap();
        let map = derive_status_map(&wf, &session, logs.path());
        assert_eq!(map["review"], NodeStatus::Failed);
    }

    #[test]
    fn status_marks_skipped_by_gate_when_log_starts_with_skipped_marker() {
        let wf = parse_standard();
        let mut session = session_with(SessionState::Completed, None);
        // implement got cost (ran) but revise has a skipped marker.
        session.record_node_cost("plan".to_string(), 0.01, 10);
        session.record_node_cost("implement".to_string(), 0.02, 11);
        session.record_node_cost("review".to_string(), 0.03, 12);
        session.record_node_cost("open_pr".to_string(), 0.04, 13);
        let logs = tempfile::tempdir().unwrap();
        std::fs::write(
            logs.path().join("revise.log"),
            "--- skipped: when `review.decision == \"changes_requested\"` evaluated false ---\n",
        )
        .unwrap();
        let map = derive_status_map(&wf, &session, logs.path());
        assert_eq!(map["revise"], NodeStatus::SkippedByGate);
        assert_eq!(map["plan"], NodeStatus::Done);
        assert_eq!(map["open_pr"], NodeStatus::Done);
    }

    #[test]
    fn build_with_missing_workflow_yaml_reports_load_error() {
        let session = session_with(SessionState::Running, None);
        let session_dir = tempfile::tempdir().unwrap();
        let workflows_root = tempfile::tempdir().unwrap();
        let progress = WorkflowProgress::build(
            &session,
            session_dir.path(),
            workflows_root.path(),
        );
        assert!(progress.load_error.is_some());
        assert!(progress.rows.is_empty());
    }
}
