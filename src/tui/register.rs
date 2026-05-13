//! Append a `projects.<key>:` block to `~/.config/fleet/agent-orchestrator.yaml`
//! as raw text. The "append-as-text" strategy preserves comments,
//! ordering, and indentation style that a `serde_yml` round-trip would
//! otherwise drop. The caller is the register-project modal in the
//! TUI; the function returns a structured error (rather than
//! mangling the yaml) when it encounters an existing layout it can't
//! safely extend — duplicate key, tabs, or an unsupported inline
//! form. The user can always fall back to `c` and edit manually.
//!
//! Only standard 2-space-indented yaml is supported. AO's example
//! configs and fleet's own scaffolding both emit 2-space indent, so
//! any user-authored file that doesn't is treated as opinionated and
//! left to manual editing.

use anyhow::{Context, Result, bail};
use std::path::Path;

/// Append a new `projects.<key>:` entry to the AO yaml at `yaml_path`.
/// Creates the file (and parent dirs) if missing, and creates the
/// `projects:` top-level key if missing. Returns an error if a
/// project with the same `key` already exists or if the projects
/// block uses tab indentation we can't safely match.
pub fn append_project_to_ao_yaml(
    yaml_path: &Path,
    key: &str,
    name: &str,
    prefix: &str,
    cwd: &Path,
) -> Result<()> {
    if let Some(parent) = yaml_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(yaml_path).unwrap_or_default();

    let summary = ProjectsBlockSummary::scan(&existing)?;
    if summary.child_keys.iter().any(|k| k == key) {
        bail!(
            "project key `{key}` already exists in {}",
            yaml_path.display()
        );
    }

    let entry = build_entry_text(key, name, prefix, cwd);

    let new_text = match summary.kind {
        ProjectsKind::Absent => append_projects_block_at_eof(&existing, &entry),
        ProjectsKind::EmptyAt {
            line_idx,
            needs_rewrite,
        } => {
            if needs_rewrite {
                replace_line_and_insert_after(&existing, line_idx, "projects:", &entry)
            } else {
                insert_after_line(&existing, line_idx, &entry)
            }
        }
        ProjectsKind::Block {
            last_child_line_idx,
        } => insert_after_line(&existing, last_child_line_idx, &entry),
    };

    std::fs::write(yaml_path, new_text)
        .with_context(|| format!("write {}", yaml_path.display()))?;
    Ok(())
}

#[derive(Debug)]
struct ProjectsBlockSummary {
    kind: ProjectsKind,
    child_keys: Vec<String>,
}

#[derive(Debug)]
enum ProjectsKind {
    /// No `projects:` top-level key in the file.
    Absent,
    /// `projects:` is present but has no children yet. `needs_rewrite`
    /// is true for inline forms (`{}`, `~`, `null`) that have to be
    /// flattened to a bare `projects:` line before insertion; false
    /// for a bare `projects:` line we can insert under directly.
    EmptyAt {
        line_idx: usize,
        needs_rewrite: bool,
    },
    /// `projects:` has one or more children; `last_child_line_idx`
    /// is the last line of the block (could be a child's last
    /// property or a comment inside the block).
    Block { last_child_line_idx: usize },
}

impl ProjectsBlockSummary {
    fn scan(text: &str) -> Result<Self> {
        let lines: Vec<&str> = text.lines().collect();
        let p_idx = lines.iter().position(|l| is_top_level_key(l, "projects"));
        let Some(p_idx) = p_idx else {
            return Ok(Self {
                kind: ProjectsKind::Absent,
                child_keys: Vec::new(),
            });
        };

        let line = lines[p_idx];
        let after_colon = line.find(':').map_or("", |c| line[c + 1..].trim());

        // Only `projects:`, `projects: {}`, `projects: ~`, `projects: null`
        // are "empty-ish" forms we can safely extend. Anything else
        // (a flow-mapping with content, a tag, etc.) gets refused so
        // we never silently rewrite user data.
        let inline_empty_marker = matches!(after_colon, "{}" | "~" | "null");
        let bare = after_colon.is_empty();
        if !inline_empty_marker && !bare {
            bail!("projects: uses an inline form (`{after_colon}`) we don't support; edit with c");
        }
        let needs_rewrite = inline_empty_marker;

        // Scan forward collecting children + the last in-block line.
        let mut child_keys = Vec::new();
        let mut last_child_line: Option<usize> = None;
        for (i, line) in lines.iter().enumerate().skip(p_idx + 1) {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with('\t') {
                bail!("ao yaml uses tab indent; edit with c");
            }
            // A non-indented line terminates the projects block (next
            // top-level key, comment at column 0, etc.).
            if !line.starts_with(' ') {
                break;
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                // In-block comment: tracks insertion site so an entry
                // doesn't land above its own header comment. Doesn't
                // count as a child key.
                last_child_line = Some(i);
                continue;
            }
            // Direct child of `projects:` lives at exactly two spaces
            // of leading whitespace; deeper lines (4+ spaces) belong
            // to whichever child we already captured.
            if let Some(rest) = line.strip_prefix("  ")
                && !rest.starts_with(' ')
                && let Some(colon) = rest.find(':')
            {
                let k = rest[..colon].trim().to_string();
                if !k.is_empty() {
                    child_keys.push(k);
                }
            }
            last_child_line = Some(i);
        }

        let kind = last_child_line.map_or(
            ProjectsKind::EmptyAt {
                line_idx: p_idx,
                needs_rewrite,
            },
            |idx| ProjectsKind::Block {
                last_child_line_idx: idx,
            },
        );
        Ok(Self { kind, child_keys })
    }
}

/// `^<key>:` matcher that ignores any trailing value/whitespace.
fn is_top_level_key(line: &str, key: &str) -> bool {
    line.strip_prefix(key)
        .is_some_and(|rest| rest.starts_with(':'))
}

/// Build the 2-space-indented yaml block for a new project entry.
/// `path` is always quoted-when-needed so a directory containing
/// spaces / colons / yaml-special chars round-trips through `serde_yml`.
fn build_entry_text(key: &str, name: &str, prefix: &str, cwd: &Path) -> String {
    let key_q = yaml_quote_scalar(key);
    let name_q = yaml_quote_scalar(name);
    let prefix_q = yaml_quote_scalar(prefix);
    let path_q = yaml_quote_scalar(&cwd.to_string_lossy());
    format!("  {key_q}:\n    name: {name_q}\n    path: {path_q}\n    sessionPrefix: {prefix_q}\n")
}

/// Returns `s` unchanged when it's a yaml "plain scalar" (no
/// whitespace, no yaml-flow / indicator chars, doesn't start with a
/// reserved leading character). Otherwise wraps in single quotes,
/// doubling any internal single quotes per yaml 1.2 §7.4.1.
fn yaml_quote_scalar(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| {
            c.is_whitespace() || matches!(c, ':' | '#' | '\'' | '"' | '[' | ']' | '{' | '}' | ',')
        })
        || s.starts_with(|c: char| {
            matches!(c, '-' | '?' | '*' | '&' | '!' | '|' | '>' | '%' | '@' | '`')
        });
    if needs_quote {
        format!("'{}'", s.replace('\'', "''"))
    } else {
        s.to_string()
    }
}

/// Append a fresh `projects:` block to `existing`. Ensures the file
/// ends with a newline and inserts a blank-line separator before the
/// new block so it reads as a distinct top-level section.
fn append_projects_block_at_eof(existing: &str, entry: &str) -> String {
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str("projects:\n");
    out.push_str(entry);
    out
}

/// Rebuild `text` inserting `insert` immediately after the line at
/// `line_idx`. Always normalises to a trailing newline.
fn insert_after_line(text: &str, line_idx: usize, insert: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        out.push_str(line);
        out.push('\n');
        if i == line_idx {
            out.push_str(insert);
        }
    }
    out
}

/// Rebuild `text` replacing the line at `line_idx` with `replacement`
/// and inserting `insert` immediately after.
fn replace_line_and_insert_after(
    text: &str,
    line_idx: usize,
    replacement: &str,
    insert: &str,
) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i == line_idx {
            out.push_str(replacement);
            out.push('\n');
            out.push_str(insert);
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_and_append(
        initial: Option<&str>,
        key: &str,
        name: &str,
        prefix: &str,
        cwd: &str,
    ) -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("agent-orchestrator.yaml");
        if let Some(body) = initial {
            std::fs::write(&path, body).unwrap();
        }
        append_project_to_ao_yaml(&path, key, name, prefix, Path::new(cwd)).expect("append ok");
        let out = std::fs::read_to_string(&path).unwrap();
        (tmp, out)
    }

    /// Round-trip the produced yaml through `serde_yml` to make sure
    /// fleet's loader will be able to parse what we wrote.
    fn assert_parses_as_ao_config(body: &str) {
        let _: crate::ao::config::AoConfig = serde_yml::from_str(body)
            .unwrap_or_else(|e| panic!("ao yaml didn't re-parse: {e}\n---\n{body}\n---"));
    }

    #[test]
    fn appends_when_file_missing() {
        let (_t, out) = write_and_append(None, "foo", "foo", "fo", "/tmp/foo");
        assert!(out.starts_with("projects:\n"));
        assert!(out.contains("  foo:\n"));
        assert!(out.contains("    path: /tmp/foo\n"));
        assert!(out.contains("    sessionPrefix: fo\n"));
        assert_parses_as_ao_config(&out);
    }

    #[test]
    fn appends_when_no_projects_key() {
        let initial = "defaults:\n  workspace: worktree\n";
        let (_t, out) = write_and_append(Some(initial), "foo", "foo", "fo", "/tmp/foo");
        // Defaults block preserved.
        assert!(out.contains("defaults:\n  workspace: worktree\n"));
        // New projects block at EOF.
        assert!(out.ends_with("    sessionPrefix: fo\n"));
        assert!(out.contains("\nprojects:\n  foo:\n"));
        assert_parses_as_ao_config(&out);
    }

    #[test]
    fn appends_when_inline_empty_projects() {
        let initial = "defaults:\n  workspace: worktree\nprojects: {}\n";
        let (_t, out) = write_and_append(Some(initial), "foo", "foo", "fo", "/tmp/foo");
        // Inline form rewritten to block form.
        assert!(!out.contains("projects: {}"));
        assert!(out.contains("projects:\n  foo:\n"));
        // Defaults preserved.
        assert!(out.contains("defaults:\n  workspace: worktree\n"));
        assert_parses_as_ao_config(&out);
    }

    #[test]
    fn appends_when_projects_has_children() {
        let initial = "\
defaults:
  workspace: worktree
projects:
  # an existing project below
  bar:
    name: bar
    path: /tmp/bar
    sessionPrefix: ba
";
        let (_t, out) = write_and_append(Some(initial), "foo", "foo", "fo", "/tmp/foo");
        // Existing project + its header comment preserved.
        assert!(out.contains("# an existing project below"));
        assert!(out.contains("  bar:\n    name: bar\n"));
        // New entry appended after bar's last property.
        let bar_pos = out.find("sessionPrefix: ba").unwrap();
        let foo_pos = out.find("  foo:").unwrap();
        assert!(foo_pos > bar_pos, "new entry must follow bar");
        assert_parses_as_ao_config(&out);
    }

    #[test]
    fn rejects_duplicate_key() {
        let initial = "\
projects:
  foo:
    name: foo
    path: /tmp/foo
    sessionPrefix: fo
";
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("agent-orchestrator.yaml");
        std::fs::write(&path, initial).unwrap();
        let err = append_project_to_ao_yaml(&path, "foo", "foo", "fo", Path::new("/tmp/foo"))
            .expect_err("duplicate must error");
        assert!(format!("{err:#}").contains("already exists"));
        // File untouched.
        let post = std::fs::read_to_string(&path).unwrap();
        assert_eq!(post, initial);
    }

    #[test]
    fn quotes_path_with_spaces() {
        let (_t, out) =
            write_and_append(None, "spaced", "Spaced Project", "sp", "/tmp/has space/dir");
        // Both name and path get single-quoted.
        assert!(out.contains("name: 'Spaced Project'"));
        assert!(out.contains("path: '/tmp/has space/dir'"));
        assert_parses_as_ao_config(&out);
    }

    #[test]
    fn rejects_tab_indent() {
        let initial = "projects:\n\tfoo:\n\t\tname: foo\n";
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("agent-orchestrator.yaml");
        std::fs::write(&path, initial).unwrap();
        let err = append_project_to_ao_yaml(&path, "bar", "bar", "ba", Path::new("/tmp/bar"))
            .expect_err("tab indent must error");
        assert!(format!("{err:#}").contains("tab indent"));
    }

    #[test]
    fn yaml_quote_passes_plain_scalar_through() {
        assert_eq!(yaml_quote_scalar("foo-bar"), "foo-bar");
        assert_eq!(yaml_quote_scalar("fo"), "fo");
    }

    #[test]
    fn yaml_quote_wraps_scalars_with_whitespace_or_specials() {
        assert_eq!(yaml_quote_scalar("has space"), "'has space'");
        assert_eq!(yaml_quote_scalar("a:b"), "'a:b'");
        assert_eq!(yaml_quote_scalar("-leading"), "'-leading'");
        // Internal single quote doubled.
        assert_eq!(yaml_quote_scalar("it's"), "'it''s'");
    }

    #[test]
    fn yaml_quote_wraps_empty_string() {
        assert_eq!(yaml_quote_scalar(""), "''");
    }
}
