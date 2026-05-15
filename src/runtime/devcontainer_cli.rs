//! Wrapper around the external `devcontainer` CLI.
//!
//! Image build and container lifecycle are delegated to the Rust
//! [`devcontainer`](https://crates.io/crates/devcontainer) crate's CLI
//! subprocess. The same CLI handles Docker and Podman (and Apple
//! Containerization, in future) — fleet's adapters pick the engine via a flag
//! and add per-backend hardening on top.
//!
//! This module owns the typed surface fleet's adapters consume; it does not
//! own host probing (that's [`super::detect`]) and it does not decide
//! hardening flags (that's the adapter — Podman appends `--runtime=runsc`
//! through [`UpRequest::extra_run_args`]; Docker passes nothing extra). The
//! helper translates fleet types into argv and parses the structured JSON the
//! CLI emits on stdout.
//!
//! A missing CLI surfaces as a single actionable error pointing at
//! `fleet runtime doctor`, rather than the raw "no such file or directory"
//! that would otherwise bubble up from a deep subprocess failure.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use super::{ContainerId, Devcontainer, ExecHandle, ExecOpts, ImageId, devcontainer::ImageSource};
use crate::process::ProcessInvoker;

/// Which container engine the devcontainer CLI should drive. Selected per
/// adapter at construction; the helper never picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Docker,
    Podman,
    /// Apple Containerization (macOS 26+). The `container` CLI is largely
    /// docker-CLI compatible at the surface the devcontainer CLI uses;
    /// per-container microVM isolation is provided by the OS, not by the
    /// devcontainer CLI or fleet.
    AppleContainer,
}

impl Engine {
    /// Executable name the devcontainer CLI shells out to under the hood.
    /// Passed via `--docker-path` because podman + Apple's `container` are
    /// both docker-CLI compatible and the devcontainer CLI defaults to
    /// running `docker`.
    pub const fn binary(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::AppleContainer => "container",
        }
    }
}

/// Inputs for [`DevcontainerCli::up`]. A struct keeps the function signature
/// stable as the spec grows (the CLI already has a dozen-plus flags for `up`).
#[derive(Debug)]
pub struct UpRequest<'a> {
    /// Repo root that owns the devcontainer.
    pub workspace: &'a Path,
    /// Optional explicit config path, when the devcontainer file lives
    /// somewhere other than the default location.
    pub config: Option<&'a Path>,
    /// Host path bind-mounted at `/artifacts` inside the container. Workflow
    /// hand-off lives here.
    pub artifacts: &'a Path,
    /// Env vars injected via `--remote-env`. The CLI persists none of these
    /// into image layers; secrets are safe here as long as the caller wipes
    /// them from process memory after the call returns.
    pub env: &'a [(String, String)],
    /// Pass-through to the engine's `run` command, e.g. `--runtime=runsc` for
    /// Podman + gVisor. Each entry becomes one `--run-args` flag.
    pub extra_run_args: &'a [String],
}

/// Typed wrapper around `devcontainer build/up/exec`. Stateless: every call
/// builds its own argv and shells out via the held [`ProcessInvoker`].
pub struct DevcontainerCli {
    invoker: Arc<dyn ProcessInvoker>,
    engine: Engine,
}

impl DevcontainerCli {
    pub fn new(invoker: Arc<dyn ProcessInvoker>, engine: Engine) -> Self {
        Self { invoker, engine }
    }

    /// Mint a stable image name from a [`Devcontainer`]. The name is the same
    /// for a given devcontainer source path + image source, across runs and
    /// processes — that's the trait's idempotency contract for `ensure_image`.
    ///
    /// Uses the non-cryptographic `DefaultHasher` because the value is an
    /// identifier, not a security boundary: collisions just mean two different
    /// devcontainers would share an image name, which a 16-hex-digit truncation
    /// makes vanishingly unlikely in practice.
    pub fn mint_image_name(dc: &Devcontainer) -> String {
        let mut hasher = DefaultHasher::new();
        dc.source_path.hash(&mut hasher);
        match &dc.image_source {
            ImageSource::Image(name) => {
                "image".hash(&mut hasher);
                name.hash(&mut hasher);
            }
            ImageSource::Build { dockerfile, context } => {
                "build".hash(&mut hasher);
                dockerfile.hash(&mut hasher);
                context.hash(&mut hasher);
            }
        }
        format!("fleet-{:016x}", hasher.finish())
    }

    /// Build the devcontainer image. Idempotent at the CLI level: the
    /// underlying engine reuses build cache layers when nothing changed.
    pub fn build(&self, workspace: &Path, dc: &Devcontainer) -> Result<ImageId> {
        let image_name = Self::mint_image_name(dc);
        let mut args = vec![
            "build".to_string(),
            "--workspace-folder".to_string(),
            workspace.display().to_string(),
            "--config".to_string(),
            dc.source_path.display().to_string(),
            "--image-name".to_string(),
            image_name.clone(),
            "--docker-path".to_string(),
            self.engine.binary().to_string(),
        ];
        // Stable arg order — tests assert on exact form.
        let _ = &mut args;

        let stdout = self.invoke(&args)?;
        let parsed: BuildResult = parse_json_output(&stdout)
            .with_context(|| format!("parsing `devcontainer build` output: {stdout}"))?;
        if parsed.outcome != "success" {
            bail!("`devcontainer build` reported non-success outcome: {}", parsed.outcome);
        }
        // The CLI echoes the `--image-name` we passed; sanity-check it. The
        // field may be a single string or an array of tags depending on CLI
        // version, so accept either via `ImageNames`.
        if !parsed.image_name.contains(&image_name) {
            bail!(
                "`devcontainer build` produced image {:?}; expected {image_name}",
                parsed.image_name
            );
        }
        Ok(ImageId::new(image_name))
    }

    /// Start (or replace) the container for the workspace. Returns the engine
    /// container id parsed from the CLI's JSON output. `--remove-existing-container`
    /// is always passed so re-entering a session reliably replaces a stale
    /// instance instead of erroring.
    pub fn up(&self, req: &UpRequest<'_>) -> Result<ContainerId> {
        let mut args = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            req.workspace.display().to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            self.engine.binary().to_string(),
        ];
        if let Some(cfg) = req.config {
            args.push("--config".to_string());
            args.push(cfg.display().to_string());
        }
        // Bind-mount the artifacts dir. The CLI accepts engine `--mount`
        // syntax verbatim via `--mount`.
        args.push("--mount".to_string());
        args.push(format!(
            "type=bind,source={},target=/artifacts",
            req.artifacts.display()
        ));
        for (k, v) in req.env {
            args.push("--remote-env".to_string());
            args.push(format!("{k}={v}"));
        }
        for extra in req.extra_run_args {
            args.push("--run-args".to_string());
            args.push(extra.clone());
        }

        let stdout = self.invoke(&args)?;
        let parsed: UpResult = parse_json_output(&stdout)
            .with_context(|| format!("parsing `devcontainer up` output: {stdout}"))?;
        if parsed.outcome != "success" {
            bail!("`devcontainer up` reported non-success outcome: {}", parsed.outcome);
        }
        Ok(ContainerId::new(parsed.container_id))
    }

    /// One-shot exec inside the workspace's container. `workdir` lands as
    /// `--remote-cwd` if set; env entries become `--remote-env`. Stdout is
    /// captured in full. Stderr and exit code are lossy under the current
    /// [`ProcessInvoker`] contract — the handle reports `exit_code: 1` plus
    /// the formatted error in `stderr` on failure, matching [`super::local`]'s
    /// behaviour.
    pub fn exec(
        &self,
        workspace: &Path,
        argv: &[String],
        opts: ExecOpts,
    ) -> Result<ExecHandle> {
        if argv.is_empty() {
            bail!("exec argv must contain at least the program name");
        }
        let mut cli_args = vec![
            "exec".to_string(),
            "--workspace-folder".to_string(),
            workspace.display().to_string(),
            "--docker-path".to_string(),
            self.engine.binary().to_string(),
        ];
        if let Some(cwd) = opts.workdir {
            cli_args.push("--remote-cwd".to_string());
            cli_args.push(cwd);
        }
        for (k, v) in opts.env {
            cli_args.push("--remote-env".to_string());
            cli_args.push(format!("{k}={v}"));
        }
        // The CLI separates its own flags from the command argv with `--`.
        cli_args.push("--".to_string());
        cli_args.extend(argv.iter().cloned());

        match self.invoke(&cli_args) {
            Ok(stdout) => Ok(ExecHandle {
                stdout,
                stderr: String::new(),
                exit_code: 0,
            }),
            Err(err) => Ok(ExecHandle {
                stdout: String::new(),
                stderr: format!("{err:#}"),
                exit_code: 1,
            }),
        }
    }

    fn invoke(&self, args: &[String]) -> Result<String> {
        self.invoker
            .run("devcontainer", args.to_vec())
            .map_err(|err| {
                // Production `RealProcessInvoker` formats ENOENT-style
                // failures as `failed to spawn ...`. Translate that into an
                // actionable hint instead of a deep io-error trail.
                let msg = format!("{err:#}");
                if msg.contains("failed to spawn `devcontainer`") {
                    anyhow::anyhow!(
                        "`devcontainer` CLI not installed — run `fleet runtime doctor` for install hints"
                    )
                } else {
                    err
                }
            })
    }
}

#[derive(Deserialize)]
struct BuildResult {
    outcome: String,
    #[serde(rename = "imageName", default)]
    image_name: ImageNames,
}

/// `devcontainer build` writes either a single string or an array of tag
/// strings into `imageName`; accept both forms transparently.
#[derive(Deserialize, Default)]
#[serde(untagged)]
enum ImageNames {
    Many(Vec<String>),
    One(String),
    #[default]
    None,
}

impl ImageNames {
    fn contains(&self, name: &str) -> bool {
        match self {
            Self::Many(v) => v.iter().any(|n| n == name),
            Self::One(n) => n == name,
            Self::None => false,
        }
    }
}

impl std::fmt::Debug for ImageNames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Many(v) => write!(f, "{v:?}"),
            Self::One(n) => write!(f, "[{n:?}]"),
            Self::None => write!(f, "[]"),
        }
    }
}

#[derive(Deserialize)]
struct UpResult {
    outcome: String,
    #[serde(rename = "containerId")]
    container_id: String,
}

/// Scan stdout for the first line that parses as the requested shape. The CLI
/// emits informational log lines on stderr by default, but some versions
/// interleave a banner on stdout above the final JSON — find the JSON line
/// rather than insisting the entire output be valid JSON.
fn parse_json_output<T: for<'de> Deserialize<'de>>(stdout: &str) -> Result<T> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<T>(line) {
            return Ok(parsed);
        }
    }
    // Fall back to parsing the whole stdout — works when the CLI emits a
    // single multi-line JSON object with no leading banner.
    serde_json::from_str(stdout).context("no parseable JSON object found in stdout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::MockProcessInvoker;
    use crate::runtime::devcontainer::Devcontainer;
    use anyhow::anyhow;
    use mockall::predicate::eq;

    fn sample_dc() -> Devcontainer {
        Devcontainer::from_str_at(
            r#"{ "image": "mcr.microsoft.com/devcontainers/rust:1" }"#,
            "/repo/.devcontainer/devcontainer.json",
        )
        .unwrap()
    }

    fn helper_with(args: Vec<String>, stdout: &'static str, engine: Engine) -> DevcontainerCli {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .with(eq("devcontainer"), eq(args))
            .returning(move |_, _| Ok(stdout.to_string()));
        DevcontainerCli::new(Arc::new(mock), engine)
    }

    #[test]
    fn engine_binary_names_match_program_names() {
        assert_eq!(Engine::Docker.binary(), "docker");
        assert_eq!(Engine::Podman.binary(), "podman");
        assert_eq!(Engine::AppleContainer.binary(), "container");
    }

    #[test]
    fn mint_image_name_is_stable_for_same_input() {
        let dc = sample_dc();
        let a = DevcontainerCli::mint_image_name(&dc);
        let b = DevcontainerCli::mint_image_name(&dc);
        assert_eq!(a, b);
        assert!(a.starts_with("fleet-"), "{a}");
    }

    #[test]
    fn mint_image_name_differs_per_source_path() {
        let dc1 = Devcontainer::from_str_at(r#"{"image":"x"}"#, "/a/devcontainer.json").unwrap();
        let dc2 = Devcontainer::from_str_at(r#"{"image":"x"}"#, "/b/devcontainer.json").unwrap();
        assert_ne!(
            DevcontainerCli::mint_image_name(&dc1),
            DevcontainerCli::mint_image_name(&dc2),
        );
    }

    #[test]
    fn mint_image_name_differs_per_image_vs_build() {
        let dc1 = Devcontainer::from_str_at(r#"{"image":"x"}"#, "/a/devcontainer.json").unwrap();
        let dc2 = Devcontainer::from_str_at(
            r#"{"build":{"dockerfile":"Dockerfile"}}"#,
            "/a/devcontainer.json",
        )
        .unwrap();
        assert_ne!(
            DevcontainerCli::mint_image_name(&dc1),
            DevcontainerCli::mint_image_name(&dc2),
        );
    }

    #[test]
    fn build_argv_includes_engine_binary_and_image_name() {
        let dc = sample_dc();
        let minted = DevcontainerCli::mint_image_name(&dc);
        let expected_args = vec![
            "build".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--config".to_string(),
            "/repo/.devcontainer/devcontainer.json".to_string(),
            "--image-name".to_string(),
            minted.clone(),
            "--docker-path".to_string(),
            "podman".to_string(),
        ];
        let stdout = format!(r#"{{"outcome":"success","imageName":"{minted}"}}"#);
        // Static lifetime escape hatch for the canned string.
        let stdout_static: &'static str = Box::leak(stdout.into_boxed_str());
        let cli = helper_with(expected_args, stdout_static, Engine::Podman);
        let id = cli.build(Path::new("/repo"), &dc).unwrap();
        assert_eq!(id.as_str(), minted);
    }

    #[test]
    fn build_accepts_image_name_as_array() {
        let dc = sample_dc();
        let minted = DevcontainerCli::mint_image_name(&dc);
        // The CLI sometimes wraps a single image name in an array.
        let stdout =
            format!(r#"{{"outcome":"success","imageName":["{minted}","extra:tag"]}}"#);
        let stdout_static: &'static str = Box::leak(stdout.into_boxed_str());
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(move |_, _| Ok(stdout_static.to_string()));
        let cli = DevcontainerCli::new(Arc::new(mock), Engine::Docker);
        let id = cli.build(Path::new("/repo"), &dc).unwrap();
        assert_eq!(id.as_str(), minted);
    }

    #[test]
    fn build_errors_when_outcome_is_not_success() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Ok(r#"{"outcome":"error","imageName":[]}"#.to_string()));
        let cli = DevcontainerCli::new(Arc::new(mock), Engine::Docker);
        let err = cli.build(Path::new("/repo"), &sample_dc()).unwrap_err();
        assert!(format!("{err}").contains("non-success outcome"));
    }

    #[test]
    fn build_errors_when_image_name_mismatches() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| {
            Ok(r#"{"outcome":"success","imageName":"some-other-name"}"#.to_string())
        });
        let cli = DevcontainerCli::new(Arc::new(mock), Engine::Docker);
        let err = cli.build(Path::new("/repo"), &sample_dc()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("produced image"), "msg = {msg}");
    }

    #[test]
    fn up_argv_includes_mounts_env_and_extra_run_args() {
        let env = vec![("ANTHROPIC_API_KEY".to_string(), "sk-test".to_string())];
        let extra = vec!["--runtime=runsc".to_string()];
        let expected_args = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "podman".to_string(),
            "--mount".to_string(),
            "type=bind,source=/sessions/s1/artifacts,target=/artifacts".to_string(),
            "--remote-env".to_string(),
            "ANTHROPIC_API_KEY=sk-test".to_string(),
            "--run-args".to_string(),
            "--runtime=runsc".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"abc123"}"#;
        let cli = helper_with(expected_args, stdout, Engine::Podman);

        let req = UpRequest {
            workspace: Path::new("/repo"),
            config: None,
            artifacts: Path::new("/sessions/s1/artifacts"),
            env: &env,
            extra_run_args: &extra,
        };
        let id = cli.up(&req).unwrap();
        assert_eq!(id.as_str(), "abc123");
    }

    #[test]
    fn up_emits_config_flag_when_provided() {
        let expected_args = vec![
            "up".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--remove-existing-container".to_string(),
            "--docker-path".to_string(),
            "docker".to_string(),
            "--config".to_string(),
            "/repo/.devcontainer/alt.json".to_string(),
            "--mount".to_string(),
            "type=bind,source=/art,target=/artifacts".to_string(),
        ];
        let stdout = r#"{"outcome":"success","containerId":"c1"}"#;
        let cli = helper_with(expected_args, stdout, Engine::Docker);
        let req = UpRequest {
            workspace: Path::new("/repo"),
            config: Some(Path::new("/repo/.devcontainer/alt.json")),
            artifacts: Path::new("/art"),
            env: &[],
            extra_run_args: &[],
        };
        cli.up(&req).unwrap();
    }

    #[test]
    fn up_errors_when_outcome_is_not_success() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run()
            .returning(|_, _| Ok(r#"{"outcome":"error","containerId":""}"#.to_string()));
        let cli = DevcontainerCli::new(Arc::new(mock), Engine::Docker);
        let req = UpRequest {
            workspace: Path::new("/repo"),
            config: None,
            artifacts: Path::new("/art"),
            env: &[],
            extra_run_args: &[],
        };
        let err = cli.up(&req).unwrap_err();
        assert!(format!("{err}").contains("non-success outcome"));
    }

    #[test]
    fn exec_requires_nonempty_argv() {
        let cli = DevcontainerCli::new(Arc::new(MockProcessInvoker::new()), Engine::Docker);
        let err = cli
            .exec(Path::new("/repo"), &[], ExecOpts::default())
            .unwrap_err();
        assert!(format!("{err}").contains("at least the program name"));
    }

    #[test]
    fn exec_argv_passes_argv_after_double_dash() {
        let expected_args = vec![
            "exec".to_string(),
            "--workspace-folder".to_string(),
            "/repo".to_string(),
            "--docker-path".to_string(),
            "docker".to_string(),
            "--remote-cwd".to_string(),
            "/workspaces/repo".to_string(),
            "--remote-env".to_string(),
            "FOO=bar".to_string(),
            "--".to_string(),
            "ls".to_string(),
            "-la".to_string(),
        ];
        let cli = helper_with(expected_args, "drwxr-xr-x  3 root root", Engine::Docker);
        let opts = ExecOpts {
            workdir: Some("/workspaces/repo".to_string()),
            env: vec![("FOO".to_string(), "bar".to_string())],
        };
        let h = cli
            .exec(Path::new("/repo"), &["ls".to_string(), "-la".to_string()], opts)
            .unwrap();
        assert_eq!(h.exit_code, 0);
        assert!(h.stdout.contains("drwxr-xr-x"));
    }

    #[test]
    fn exec_returns_lossy_handle_on_invoker_error() {
        let mut mock = MockProcessInvoker::new();
        mock.expect_run().returning(|_, _| Err(anyhow!("boom")));
        let cli = DevcontainerCli::new(Arc::new(mock), Engine::Docker);
        let h = cli
            .exec(Path::new("/repo"), &["false".to_string()], ExecOpts::default())
            .unwrap();
        assert_eq!(h.exit_code, 1);
        assert!(h.stderr.contains("boom"));
    }

    #[test]
    fn missing_cli_surfaces_as_actionable_error() {
        let mut mock = MockProcessInvoker::new();
        // Mirror `RealProcessInvoker`'s wording so the helper's
        // pattern-match recognises the ENOENT shape.
        mock.expect_run().returning(|_, _| {
            Err(anyhow!(
                "failed to spawn `devcontainer`: No such file or directory (os error 2)"
            ))
        });
        let cli = DevcontainerCli::new(Arc::new(mock), Engine::Podman);
        let err = cli.build(Path::new("/repo"), &sample_dc()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fleet runtime doctor"),
            "expected doctor hint, got: {msg}"
        );
    }

    #[test]
    fn parse_json_output_finds_json_after_leading_banner() {
        let stdout = "[info] starting up\nrandom log\n{\"outcome\":\"success\",\"containerId\":\"c1\"}";
        let parsed: UpResult = parse_json_output(stdout).unwrap();
        assert_eq!(parsed.outcome, "success");
        assert_eq!(parsed.container_id, "c1");
    }

    #[test]
    fn parse_json_output_errors_when_no_json_line_present() {
        let stdout = "no json here";
        let parsed: Result<UpResult> = parse_json_output(stdout);
        assert!(parsed.is_err());
    }
}
