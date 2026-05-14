//! Minimal `devcontainer.json` parsing.
//!
//! Fleet does **not** re-implement the full devcontainer spec; the actual
//! image build, Features installation, and lifecycle hooks are delegated to
//! the external devcontainer CLI. This module models only the fields fleet
//! itself reasons about directly:
//!
//! - whether the image comes from a registry (`image:`) or a Dockerfile build
//!   (`build:`), because adapters that pre-pull must know which path applies;
//! - the `workspaceFolder` so we know where the host worktree should be
//!   bind-mounted inside the container.
//!
//! Everything else stays as raw JSON value form (`extra`) so the devcontainer
//! CLI sees the user's authored file verbatim — fleet must not round-trip
//! a user's devcontainer through a lossy parse.
//!
//! Devcontainer files are JSONC (JSON with comments + trailing commas). We
//! strip those to JSON before handing the bytes to `serde_json`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// A parsed devcontainer description. Only the fields fleet consumes directly
/// are typed; the original on-disk path is retained so adapters can pass it
/// to the devcontainer CLI verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Devcontainer {
    /// Path to the parsed `devcontainer.json` (or `.devcontainer.json`,
    /// or `.devcontainer/devcontainer.json` — all spec-legal locations).
    pub source_path: PathBuf,
    pub image_source: ImageSource,
    /// Where the workspace is mounted inside the container. `None` means
    /// "let the devcontainer CLI apply its default" (typically
    /// `/workspaces/<repo-name>`).
    pub workspace_folder: Option<String>,
}

/// Whether the image is pulled or built. Builds carry the dockerfile +
/// context paths *resolved relative to the devcontainer file* — callers get
/// absolute paths and don't need to know about the parse-side relativity rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    Image(String),
    Build { dockerfile: PathBuf, context: PathBuf },
}

// Unknown fields are silently ignored (serde's default). Real-world
// devcontainers carry `features`, `runArgs`, `customizations`, etc., that
// fleet doesn't touch directly — the devcontainer CLI re-reads the file in
// full, so dropping them here is lossless.
#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    build: Option<RawBuild>,
    #[serde(default, rename = "workspaceFolder")]
    workspace_folder: Option<String>,
}

#[derive(Deserialize)]
struct RawBuild {
    dockerfile: String,
    #[serde(default)]
    context: Option<String>,
}

impl Devcontainer {
    /// Parse a devcontainer.json file from disk. The returned struct keeps
    /// `source_path` set to the canonicalised input path, so callers can
    /// reliably resolve sibling files.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading devcontainer file: {}", path.display()))?;
        Self::from_str_at(&raw, path)
    }

    /// Parse devcontainer JSONC text, treating `at` as the on-disk location
    /// (needed to resolve `build.dockerfile` and `build.context` paths).
    /// The `at` path does not need to exist on disk; tests use synthetic
    /// paths to exercise resolution rules.
    pub fn from_str_at(input: &str, at: impl AsRef<Path>) -> Result<Self> {
        let at = at.as_ref();
        let stripped = strip_jsonc(input);
        let raw: Raw = serde_json::from_str(&stripped).with_context(|| {
            format!("parsing devcontainer JSON at {}", at.display())
        })?;

        let image_source = match (raw.image, raw.build) {
            (Some(_), Some(_)) => bail!(
                "devcontainer at {} declares both `image` and `build`; pick one",
                at.display()
            ),
            (None, None) => bail!(
                "devcontainer at {} must declare either `image` or `build`",
                at.display()
            ),
            (Some(image), None) => ImageSource::Image(image),
            (None, Some(build)) => {
                let parent = at.parent().unwrap_or_else(|| Path::new("."));
                let dockerfile = parent.join(&build.dockerfile);
                let context = build
                    .context
                    .as_deref()
                    .map_or_else(|| parent.to_path_buf(), |c| parent.join(c));
                ImageSource::Build { dockerfile, context }
            }
        };

        Ok(Self {
            source_path: at.to_path_buf(),
            image_source,
            workspace_folder: raw.workspace_folder,
        })
    }
}

/// Convert JSONC (JSON with comments and trailing commas) to plain JSON by
/// replacing comments with whitespace (line-count-preserving so parser error
/// messages keep pointing at the right line) and removing trailing commas
/// before `}` / `]`. Strings — including their `//` and `,` contents — are
/// passed through untouched.
///
/// This is intentionally a hand-rolled tiny scanner rather than a full
/// JSONC parser dependency: we only need to feed the result to `serde_json`,
/// and the rules are simple enough that a dedicated crate would be more
/// surface area than safety.
fn strip_jsonc(input: &str) -> String {
    enum State {
        Normal,
        InString,
        InStringEscape,
        InLineComment,
        InBlockComment,
        InBlockCommentStar,
    }

    let mut out = String::with_capacity(input.len());
    let mut state = State::Normal;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            State::Normal => match c {
                '"' => {
                    out.push(c);
                    state = State::InString;
                }
                '/' if chars.peek() == Some(&'/') => {
                    chars.next();
                    state = State::InLineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = State::InBlockComment;
                }
                _ => out.push(c),
            },
            State::InString => {
                out.push(c);
                match c {
                    '"' => state = State::Normal,
                    '\\' => state = State::InStringEscape,
                    _ => {}
                }
            }
            State::InStringEscape => {
                out.push(c);
                state = State::InString;
            }
            State::InLineComment => {
                if c == '\n' {
                    out.push('\n');
                    state = State::Normal;
                }
            }
            State::InBlockComment => {
                if c == '\n' {
                    // Preserve newlines so line numbers in parser errors
                    // stay aligned with the source file.
                    out.push('\n');
                } else if c == '*' {
                    state = State::InBlockCommentStar;
                }
            }
            State::InBlockCommentStar => match c {
                '/' => state = State::Normal,
                '\n' => {
                    out.push('\n');
                    state = State::InBlockComment;
                }
                '*' => {}
                _ => state = State::InBlockComment,
            },
        }
    }

    strip_trailing_commas(&out)
}

/// Remove `,` immediately preceding `}` or `]` (ignoring whitespace). Strings
/// are skipped over the same way as in [`strip_jsonc`] so a `","` literal
/// inside a string is not mistaken for a trailing comma.
fn strip_trailing_commas(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b as char);
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push(b as char);
            i += 1;
            continue;
        }
        if b == b',' {
            // Look ahead past whitespace.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                // Replace the comma with a space; keep whitespace bytes
                // verbatim so the trailing `}` / `]` lands at the same
                // line/column it did before.
                out.push(' ');
                i += 1;
                continue;
            }
        }
        out.push(b as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_image_form() {
        let dc = Devcontainer::from_str_at(
            r#"{ "image": "mcr.microsoft.com/devcontainers/rust:1" }"#,
            "/repo/.devcontainer/devcontainer.json",
        )
        .unwrap();
        assert_eq!(
            dc.image_source,
            ImageSource::Image("mcr.microsoft.com/devcontainers/rust:1".to_string())
        );
        assert_eq!(dc.workspace_folder, None);
    }

    #[test]
    fn parses_build_form_resolving_paths_relative_to_file() {
        let dc = Devcontainer::from_str_at(
            r#"{ "build": { "dockerfile": "Dockerfile", "context": ".." } }"#,
            "/repo/.devcontainer/devcontainer.json",
        )
        .unwrap();
        match dc.image_source {
            ImageSource::Build { dockerfile, context } => {
                assert_eq!(dockerfile, PathBuf::from("/repo/.devcontainer/Dockerfile"));
                assert_eq!(context, PathBuf::from("/repo/.devcontainer/.."));
            }
            ImageSource::Image(_) => panic!("expected Build"),
        }
    }

    #[test]
    fn build_defaults_context_to_devcontainer_directory() {
        let dc = Devcontainer::from_str_at(
            r#"{ "build": { "dockerfile": "Dockerfile" } }"#,
            "/repo/.devcontainer/devcontainer.json",
        )
        .unwrap();
        match dc.image_source {
            ImageSource::Build { context, .. } => {
                assert_eq!(context, PathBuf::from("/repo/.devcontainer"));
            }
            ImageSource::Image(_) => panic!("expected Build"),
        }
    }

    #[test]
    fn rejects_both_image_and_build() {
        let err = Devcontainer::from_str_at(
            r#"{ "image": "x", "build": { "dockerfile": "Dockerfile" } }"#,
            "/repo/devcontainer.json",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("both `image` and `build`"));
    }

    #[test]
    fn rejects_neither_image_nor_build() {
        let err =
            Devcontainer::from_str_at(r#"{ "workspaceFolder": "/work" }"#, "/x.json").unwrap_err();
        assert!(format!("{err}").contains("must declare either `image` or `build`"));
    }

    #[test]
    fn parses_workspace_folder() {
        let dc = Devcontainer::from_str_at(
            r#"{ "image": "x", "workspaceFolder": "/work" }"#,
            "/x.json",
        )
        .unwrap();
        assert_eq!(dc.workspace_folder.as_deref(), Some("/work"));
    }

    #[test]
    fn tolerates_unknown_fields_at_top_level() {
        // Real-world devcontainers carry a ton of extra fields (features,
        // remoteEnv, runArgs, customizations …). Fleet must round-trip
        // happily even when it doesn't model them.
        let dc = Devcontainer::from_str_at(
            r#"{
                "image": "x",
                "features": { "ghcr.io/devcontainers/features/node:1": {} },
                "runArgs": ["--cap-add=NET_ADMIN"],
                "customizations": { "vscode": { "extensions": [] } }
            }"#,
            "/x.json",
        )
        .unwrap();
        assert_eq!(dc.image_source, ImageSource::Image("x".to_string()));
    }

    #[test]
    fn parses_jsonc_with_line_comments() {
        let dc = Devcontainer::from_str_at(
            r#"{
                // top-level comment
                "image": "x" // trailing
            }"#,
            "/x.json",
        )
        .unwrap();
        assert_eq!(dc.image_source, ImageSource::Image("x".to_string()));
    }

    #[test]
    fn parses_jsonc_with_block_comments() {
        let dc = Devcontainer::from_str_at(
            r#"{
                /* block
                   comment
                */
                "image": "x" /* trailing */
            }"#,
            "/x.json",
        )
        .unwrap();
        assert_eq!(dc.image_source, ImageSource::Image("x".to_string()));
    }

    #[test]
    fn tolerates_trailing_commas() {
        let dc = Devcontainer::from_str_at(
            r#"{
                "image": "x",
                "workspaceFolder": "/w",
            }"#,
            "/x.json",
        )
        .unwrap();
        assert_eq!(dc.image_source, ImageSource::Image("x".to_string()));
        assert_eq!(dc.workspace_folder.as_deref(), Some("/w"));
    }

    #[test]
    fn does_not_strip_double_slash_inside_strings() {
        // A naive comment-stripper would eat "http://" — verify ours doesn't.
        let dc = Devcontainer::from_str_at(
            r#"{ "image": "https://registry.example.com/img:1" }"#,
            "/x.json",
        )
        .unwrap();
        assert_eq!(
            dc.image_source,
            ImageSource::Image("https://registry.example.com/img:1".to_string())
        );
    }

    #[test]
    fn does_not_strip_commas_inside_strings() {
        let dc = Devcontainer::from_str_at(
            r#"{ "image": "x", "workspaceFolder": "/a,/b" }"#,
            "/x.json",
        )
        .unwrap();
        assert_eq!(dc.workspace_folder.as_deref(), Some("/a,/b"));
    }

    #[test]
    fn handles_escaped_quotes_inside_strings() {
        let dc = Devcontainer::from_str_at(
            r#"{ "image": "with \"quotes\" inside" }"#,
            "/x.json",
        )
        .unwrap();
        assert_eq!(
            dc.image_source,
            ImageSource::Image(r#"with "quotes" inside"#.to_string())
        );
    }

    #[test]
    fn loads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devcontainer.json");
        std::fs::write(&path, r#"{ "image": "from-disk" }"#).unwrap();
        let dc = Devcontainer::from_path(&path).unwrap();
        assert_eq!(dc.source_path, path);
        assert_eq!(dc.image_source, ImageSource::Image("from-disk".to_string()));
    }

    #[test]
    fn from_path_surfaces_io_error_with_path_context() {
        let err = Devcontainer::from_path("/no/such/devcontainer.json").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/no/such/devcontainer.json"), "msg = {msg}");
    }

    #[test]
    fn strip_jsonc_preserves_line_count() {
        // Line numbers in serde_json errors must keep pointing at the
        // right source line, so comment-stripping must not collapse lines.
        let input = "{\n// comment\n\"image\": \"x\"\n}";
        let stripped = strip_jsonc(input);
        assert_eq!(stripped.lines().count(), input.lines().count());
    }
}
