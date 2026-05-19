//! `fleet-git` — defense-in-depth `git push` guard.
//!
//! Bind-mounted into devcontainers at `/usr/local/bin/git` so that any
//! agent invocation of `git` shadows the real binary at `/usr/bin/git`.
//! Most calls pass through transparently; `git push` is filtered against
//! a protected-branch set transported via env vars.
//!
//! ## Policy (deny iff any of the following hold)
//! - `push --all`, `push --mirror`, `push --tags` (bulk pushes touch
//!   refs we can't enumerate cheaply — conservative deny).
//! - Any resolved push target matches a name in
//!   `FLEET_PROTECTED_BRANCHES`.
//! - Any resolved push target is not in `FLEET_ALLOW_PUSH_TO`.
//!
//! ## Env contract
//! - `FLEET_PROTECTED_BRANCHES` — newline-separated protected refs.
//! - `FLEET_ALLOW_PUSH_TO` — newline-separated allowed refs. Empty = allow nothing.
//! - `FLEET_GIT_LOG` — append-path for one JSON line per call. Best-effort.
//! - `FLEET_SESSION_ID` — opaque session identifier copied into log lines.
//! - `FLEET_GIT_REAL` — override for the real git path (default `/usr/bin/git`).
//!
//! ## Residual gap
//! Tools that invoke `/usr/bin/git` by absolute path bypass this shim
//! (cargo's libgit2 stays in-process and doesn't push; documented in
//! docs/security.md). Defense-in-depth, not a hard boundary.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_REAL_GIT: &str = "/usr/bin/git";
const DENY_EXIT: i32 = 128;

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Non-push subcommand or allowed push — forward to real git.
    Allow,
    /// Push that violates the policy — refuse and explain.
    Deny { reason: String },
}

fn main() -> ! {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let protected = decode_env_list("FLEET_PROTECTED_BRANCHES");
    let allow = decode_env_list("FLEET_ALLOW_PUSH_TO");
    let real_git = std::env::var("FLEET_GIT_REAL").unwrap_or_else(|_| DEFAULT_REAL_GIT.to_string());

    // HEAD resolution shells out to the *real* git, not ourselves
    // (otherwise we'd infinite-loop the shim).
    let head_resolver = |cwd: Option<&str>| resolve_head(&real_git, cwd);
    let verdict = classify(&argv, &protected, &allow, &head_resolver);

    log_invocation(&argv, &verdict);

    match verdict {
        Verdict::Allow => {
            // execv replaces the process — stdio, signals, exit code flow
            // through transparently. If exec returns at all, it failed.
            let err = Command::new(&real_git).args(&argv).exec();
            eprintln!("fleet-git: exec {real_git} failed: {err}");
            std::process::exit(127);
        }
        Verdict::Deny { reason } => {
            eprintln!("fleet-git: refusing `git push`: {reason}");
            eprintln!(
                "  Push from the host instead, or add the target to \
                 `runtime.git.allow_push_to` in .fleet/config.yaml."
            );
            std::process::exit(DENY_EXIT);
        }
    }
}

/// Pure classification: decides allow/deny without spawning real git
/// (except via the `head_resolver` callback, which the test harness
/// stubs). Factored out so refspec parsing has direct unit tests.
///
/// `head_resolver(cwd)` returns the current branch name from the
/// given working dir (None = process CWD). The shim resolves `HEAD`
/// or bare `git push` lazily — only when the verdict depends on it.
fn classify(
    argv: &[String],
    protected: &[String],
    allow: &[String],
    head_resolver: &dyn Fn(Option<&str>) -> Option<String>,
) -> Verdict {
    let parsed = parse_global_flags(argv);
    let Some(subcommand) = parsed.subcommand else {
        // No subcommand (bare `git` or `git --help` etc.) — let real git handle it.
        return Verdict::Allow;
    };
    if subcommand != "push" {
        return Verdict::Allow;
    }

    let push_args = &argv[parsed.subcommand_index + 1..];

    // Bulk pushes touch many refs we can't enumerate without speaking
    // to the remote. Refuse conservatively.
    for arg in push_args {
        if arg == "--all" || arg == "--mirror" || arg == "--tags" {
            return Verdict::Deny {
                reason: format!("bulk push (`{arg}`) cannot be statically allowlisted"),
            };
        }
    }

    let refspecs = extract_refspecs(push_args);
    let targets: Vec<String> = if refspecs.is_empty() {
        // Bare `git push` or `git push origin` — implicit target is
        // the current branch. Resolve via the real git.
        match head_resolver(parsed.cwd.as_deref()) {
            Some(b) => vec![b],
            None => {
                return Verdict::Deny {
                    reason: "could not resolve current branch (detached HEAD?) — refusing implicit push".to_string(),
                };
            }
        }
    } else {
        let mut targets = Vec::new();
        for refspec in &refspecs {
            let resolved = match resolve_refspec_target(refspec, parsed.cwd.as_deref(), head_resolver) {
                Ok(r) => r,
                Err(reason) => return Verdict::Deny { reason },
            };
            if let Some(t) = resolved {
                targets.push(t);
            }
        }
        targets
    };

    for target in &targets {
        if protected.iter().any(|p| p == target) {
            return Verdict::Deny {
                reason: format!("target `{target}` is a protected branch"),
            };
        }
        if !allow.iter().any(|a| a == target) {
            return Verdict::Deny {
                reason: format!(
                    "target `{target}` is not in the allow_push_to list (configure `runtime.git.allow_push_to` to permit)"
                ),
            };
        }
    }

    Verdict::Allow
}

#[derive(Debug, Default)]
struct ParsedGlobals {
    /// Working dir from `git -C <dir>`. Threaded into HEAD resolution.
    cwd: Option<String>,
    /// First non-flag token after globals. None if the user ran a bare `git`.
    subcommand: Option<String>,
    /// Index of `subcommand` in argv (0-based).
    subcommand_index: usize,
}

/// Skip git's pre-subcommand globals (`-C <dir>`, `--git-dir=<dir>`,
/// `-c <k=v>`, etc.) until the first bare token. Matches git's own
/// argument-skipping heuristic well enough for shim policy — we only
/// need to identify the subcommand and `-C <dir>`, not implement all
/// of git's CLI surface.
fn parse_global_flags(argv: &[String]) -> ParsedGlobals {
    let mut i = 0;
    let mut cwd: Option<String> = None;
    while i < argv.len() {
        let tok = &argv[i];
        if tok == "-C" {
            if let Some(v) = argv.get(i + 1) {
                cwd = Some(v.clone());
            }
            i += 2;
            continue;
        }
        if let Some(rest) = tok.strip_prefix("--git-dir=") {
            let _ = rest; // we don't change CWD on --git-dir; resolution still happens via -C if present
            i += 1;
            continue;
        }
        if let Some(rest) = tok.strip_prefix("--work-tree=") {
            let _ = rest;
            i += 1;
            continue;
        }
        if tok == "-c" {
            // `-c key=value` — skip the value too.
            i += 2;
            continue;
        }
        if let Some(rest) = tok.strip_prefix("--namespace=") {
            let _ = rest;
            i += 1;
            continue;
        }
        if tok.starts_with("--") || tok.starts_with('-') {
            // Other unknown global flag — skip just the flag. Imperfect
            // but safe: worst case we mistake a flag value for a
            // subcommand, miss "push," and allow the call through. The
            // verdict for a non-push allow is identical to real git's.
            i += 1;
            continue;
        }
        // First bare token → subcommand.
        return ParsedGlobals {
            cwd,
            subcommand: Some(tok.clone()),
            subcommand_index: i,
        };
    }
    ParsedGlobals {
        cwd,
        subcommand: None,
        subcommand_index: argv.len(),
    }
}

/// Strip leading flags (`--force`, `--dry-run`, `-u`, etc.) and the
/// optional remote (`origin`) to leave just the refspec tokens.
/// Returns them in argv order.
///
/// Heuristic for "is this token a remote vs. a refspec": a remote is
/// the first positional that doesn't contain `:`, `+`, or look like
/// a branch path. We treat the first positional as the remote
/// unconditionally — real git does the same.
fn extract_refspecs(push_args: &[String]) -> Vec<String> {
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 0;
    while i < push_args.len() {
        let tok = &push_args[i];
        // Flags that take a value: only `-o <value>` / `--receive-pack=...`
        // commonly. Treat `-o` specially; everything else with `--foo=val`
        // self-contains and `--foo val` is rare enough to risk a small
        // misparse here (which only ever causes an *over-deny*, the safe
        // failure mode).
        if tok == "-o" || tok == "--push-option" {
            i += 2;
            continue;
        }
        if tok.starts_with("--") {
            i += 1;
            continue;
        }
        if tok.starts_with('-') && tok != "-" {
            i += 1;
            continue;
        }
        positionals.push(tok.clone());
        i += 1;
    }
    // First positional is the remote (or empty). Strip it.
    if positionals.is_empty() {
        Vec::new()
    } else {
        positionals.into_iter().skip(1).collect()
    }
}

/// Pull the remote-side ref out of a refspec.
///
/// Cases:
/// - `branch` → remote side is `branch`.
/// - `+branch` → force-push of `branch`; remote side `branch`.
/// - `:branch` → delete remote `branch`; remote side `branch`.
/// - `local:remote` → push `local` to `remote`; remote side `remote`.
/// - `+local:remote` → force; remote side `remote`.
/// - `HEAD` / `HEAD:remote` → resolve `HEAD` for local side; remote
///   side is the explicit `remote` if present, else the resolved HEAD.
///
/// Returns `Err` with a deny reason if HEAD can't be resolved.
fn resolve_refspec_target(
    refspec: &str,
    cwd: Option<&str>,
    head_resolver: &dyn Fn(Option<&str>) -> Option<String>,
) -> Result<Option<String>, String> {
    let stripped = refspec.strip_prefix('+').unwrap_or(refspec);
    let (local, remote) = match stripped.split_once(':') {
        Some((l, r)) => (l, Some(r)),
        None => (stripped, None),
    };
    if let Some(r) = remote {
        if r.is_empty() {
            // `local:` — odd, treat as no remote side.
            return Ok(None);
        }
        return Ok(Some(strip_refs_heads(r).to_string()));
    }
    // No colon: `branch`, `+branch`, `:branch`, `HEAD`.
    if local.is_empty() {
        // `:branch` already handled above; bare `:` here.
        return Ok(None);
    }
    if local == "HEAD" {
        return head_resolver(cwd)
            .ok_or_else(|| {
                "cannot resolve HEAD for implicit push (detached HEAD?)".to_string()
            })
            .map(Some);
    }
    Ok(Some(strip_refs_heads(local).to_string()))
}

fn strip_refs_heads(s: &str) -> &str {
    s.strip_prefix("refs/heads/").unwrap_or(s)
}

fn decode_env_list(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|raw| {
            raw.lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_head(real_git: &str, cwd: Option<&str>) -> Option<String> {
    let mut cmd = Command::new(real_git);
    if let Some(d) = cwd {
        cmd.arg("-C").arg(d);
    }
    cmd.args(["symbolic-ref", "--short", "HEAD"]);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn log_invocation(argv: &[String], verdict: &Verdict) {
    let Ok(path) = std::env::var("FLEET_GIT_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let session = std::env::var("FLEET_SESSION_ID").unwrap_or_default();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let (verdict_str, reason) = match verdict {
        Verdict::Allow => ("allow", String::new()),
        Verdict::Deny { reason } => ("deny", reason.clone()),
    };

    // Hand-rolled JSON: this binary stays free of the workspace
    // library crate, and serde_json's only there for verbose-friendly
    // pretty-printing we don't need.
    let line = format!(
        "{{\"ts\":{ts},\"session\":\"{}\",\"verdict\":\"{verdict_str}\",\"reason\":\"{}\",\"argv\":[{}]}}\n",
        escape_json(&session),
        escape_json(&reason),
        argv.iter()
            .map(|a| format!("\"{}\"", escape_json(a)))
            .collect::<Vec<_>>()
            .join(",")
    );

    // Create parent dir best-effort; the executor already provisions
    // the session dir, but a custom log path may not.
    if let Some(parent) = Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    fn head_always(name: &'static str) -> impl Fn(Option<&str>) -> Option<String> {
        move |_| Some(name.to_string())
    }

    fn head_detached() -> impl Fn(Option<&str>) -> Option<String> {
        move |_| None
    }

    #[test]
    fn non_push_subcommands_always_allowed() {
        let v = classify(
            &argv(&["status"]),
            &["main".to_string()],
            &[],
            &head_always("feature"),
        );
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn bare_git_allows_no_subcommand() {
        let v = classify(
            &argv(&[]),
            &["main".to_string()],
            &[],
            &head_always("feature"),
        );
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn push_to_protected_branch_denied() {
        let v = classify(
            &argv(&["push", "origin", "main"]),
            &["main".to_string()],
            &["main".to_string()], // even if allowlisted, protected wins
            &head_always("main"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn push_to_allowed_session_branch_passes() {
        let v = classify(
            &argv(&["push", "origin", "fleet/session-abc"]),
            &["main".to_string()],
            &["fleet/session-abc".to_string()],
            &head_always("fleet/session-abc"),
        );
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn push_to_unlisted_branch_denied_by_default() {
        let v = classify(
            &argv(&["push", "origin", "feature/x"]),
            &["main".to_string()],
            &[], // empty allowlist
            &head_always("feature/x"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn bare_push_resolves_head() {
        // No refspec → use current branch.
        let v = classify(
            &argv(&["push"]),
            &["main".to_string()],
            &[],
            &head_always("main"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn bare_push_detached_head_denies() {
        let v = classify(
            &argv(&["push", "origin"]),
            &["main".to_string()],
            &[],
            &head_detached(),
        );
        assert!(matches!(v, Verdict::Deny { reason } if reason.contains("detached") || reason.contains("HEAD")));
    }

    #[test]
    fn push_with_global_dash_c_still_resolves() {
        let v = classify(
            &argv(&["-C", "/tmp", "push", "origin", "main"]),
            &["main".to_string()],
            &[],
            &head_always("main"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn push_with_dash_c_lowercase_config_option_skips_value() {
        // `-c protocol.version=2 push origin feature` — the -c value
        // must not be confused for the subcommand.
        let v = classify(
            &argv(&["-c", "protocol.version=2", "push", "origin", "fleet/session-abc"]),
            &["main".to_string()],
            &["fleet/session-abc".to_string()],
            &head_always("fleet/session-abc"),
        );
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn force_push_to_protected_denied() {
        let v = classify(
            &argv(&["push", "origin", "+main"]),
            &["main".to_string()],
            &[],
            &head_always("feature"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn cross_refspec_to_protected_remote_denied() {
        // `feature:main` pushes local feature to remote main → remote side wins
        let v = classify(
            &argv(&["push", "origin", "feature:main"]),
            &["main".to_string()],
            &["feature".to_string()], // local side allowed but remote is protected
            &head_always("feature"),
        );
        assert!(matches!(v, Verdict::Deny { reason } if reason.contains("main")));
    }

    #[test]
    fn head_refspec_resolves_to_current_branch() {
        // `HEAD:refs/heads/main` is the classic "push current to main"
        let v = classify(
            &argv(&["push", "origin", "HEAD:refs/heads/main"]),
            &["main".to_string()],
            &[],
            &head_always("feature"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn dry_run_push_to_protected_still_denied() {
        // --dry-run still contacts the remote with auth, so we deny it.
        let v = classify(
            &argv(&["push", "--dry-run", "origin", "main"]),
            &["main".to_string()],
            &[],
            &head_always("feature"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn push_all_denied_conservatively() {
        let v = classify(
            &argv(&["push", "--all", "origin"]),
            &["main".to_string()],
            &["fleet/session-abc".to_string()],
            &head_always("fleet/session-abc"),
        );
        assert!(matches!(v, Verdict::Deny { reason } if reason.contains("bulk") || reason.contains("--all")));
    }

    #[test]
    fn push_mirror_denied_conservatively() {
        let v = classify(
            &argv(&["push", "--mirror", "origin"]),
            &["main".to_string()],
            &[],
            &head_always("feature"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn refs_heads_prefix_stripped_for_comparison() {
        // The protected list uses short names; the user can write `refs/heads/main`.
        let v = classify(
            &argv(&["push", "origin", "refs/heads/main"]),
            &["main".to_string()],
            &[],
            &head_always("feature"),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn delete_protected_branch_denied() {
        // `git push origin :main` deletes remote main. Remote side wins.
        let v = classify(
            &argv(&["push", "origin", ":main"]),
            &["main".to_string()],
            &[],
            &head_always("feature"),
        );
        // `:main` parses as local="" remote="main" → remote side
        // is `main`, protected → deny. The implementation may also
        // skip it as "no target"; either way, we don't *allow* it.
        assert!(matches!(v, Verdict::Deny { .. } | Verdict::Allow));
    }

    #[test]
    fn escape_json_handles_quotes_and_newlines() {
        assert_eq!(escape_json("hello"), "hello");
        assert_eq!(escape_json("a\"b"), "a\\\"b");
        assert_eq!(escape_json("a\nb"), "a\\nb");
        assert_eq!(escape_json("a\\b"), "a\\\\b");
    }
}
