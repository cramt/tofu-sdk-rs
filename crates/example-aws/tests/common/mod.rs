//! Shared helpers for the OpenTofu/Terraform contract tests: locate the engine,
//! lay out a `dev_overrides` workspace pointing at the built provider binary,
//! and run engine commands in it.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// Find the CLI binary, preferring OpenTofu. Panics if neither is installed —
/// these tests deliberately require a real engine.
pub fn engine() -> String {
    for candidate in ["tofu", "terraform"] {
        if Command::new(candidate)
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return candidate.to_string();
        }
    }
    panic!(
        "these contract tests require `tofu` or `terraform` on PATH \
         (enter the nix dev shell: `nix develop`)"
    );
}

/// A self-cleaning temp directory.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("tofu-sdk-rs-it-{}-{}", std::process::id(), nanos));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A `dev_overrides` workspace: the provider symlinked under a plugins dir, a
/// CLI config pointing at it, and a config directory holding `main.tf`.
pub struct Workspace {
    _dir: TempDir,
    pub tofurc: PathBuf,
    pub cfg: PathBuf,
}

/// Build an empty workspace (provider symlinked, CLI config written, empty
/// config dir) ready to be populated with config files.
fn empty_workspace() -> Workspace {
    let provider_bin = env!("CARGO_BIN_EXE_example-aws");
    let dir = TempDir::new();

    let plugins = dir.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    symlink(provider_bin, plugins.join("terraform-provider-aws")).expect("symlink provider");

    let tofurc = dir.path().join("tofurc");
    fs::write(
        &tofurc,
        format!(
            "provider_installation {{\n  dev_overrides {{\n    \"example/aws\" = {plugins:?}\n  }}\n  direct {{}}\n}}\n"
        ),
    )
    .unwrap();

    let cfg = dir.path().join("cfg");
    fs::create_dir_all(&cfg).unwrap();

    Workspace {
        _dir: dir,
        tofurc,
        cfg,
    }
}

/// Build a workspace whose `main.tf` is `config_tf`, wired to the example
/// provider binary via `dev_overrides` (so no `init` is needed).
pub fn workspace(config_tf: &str) -> Workspace {
    let ws = empty_workspace();
    fs::write(ws.cfg.join("main.tf"), config_tf).unwrap();
    ws
}

/// Build a workspace by copying every file from `fixtures_dir` (a directory of
/// `.tf` / `.tftest.hcl` files) into the config dir. Used to run a `tofu test`
/// suite that lives in the repo under `tests/tofu/`.
pub fn workspace_from_fixtures(fixtures_dir: &Path) -> Workspace {
    let ws = empty_workspace();
    for entry in fs::read_dir(fixtures_dir).expect("read fixtures dir") {
        let entry = entry.expect("fixtures dir entry");
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            fs::copy(entry.path(), ws.cfg.join(entry.file_name())).expect("copy fixture file");
        }
    }
    ws
}

/// Run an engine command in the workspace.
pub fn run(engine: &str, args: &[&str], ws: &Workspace) -> Output {
    Command::new(engine)
        .args(args)
        .current_dir(&ws.cfg)
        .env("TF_CLI_CONFIG_FILE", &ws.tofurc)
        .output()
        .expect("run engine command")
}

/// Assert a command succeeded, printing stdout/stderr on failure.
pub fn assert_ok(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- JSON navigation over the engine's `-json` output -----------------------
//
// The engine emits dynamic JSON; we parse it into a `facet_value::Value` and
// walk it with these helpers (no serde).

pub use facet_value::Value as Json;

/// Parse engine JSON output into a dynamic value.
pub fn json(bytes: &[u8]) -> Json {
    facet_json::from_slice(bytes).expect("engine emitted valid JSON")
}

/// Follow a path of object keys, returning `None` if any segment is missing or
/// not an object.
pub fn path<'a>(value: &'a Json, keys: &[&str]) -> Option<&'a Json> {
    let mut current = value;
    for key in keys {
        current = current.as_object()?.get(key)?;
    }
    Some(current)
}

/// Follow a path of object keys, panicking with the path on any miss.
pub fn get<'a>(value: &'a Json, keys: &[&str]) -> &'a Json {
    path(value, keys).unwrap_or_else(|| panic!("missing JSON path {keys:?}"))
}

/// The string at `keys` (panics if absent or not a string).
pub fn string<'a>(value: &'a Json, keys: &[&str]) -> &'a str {
    get(value, keys)
        .as_string()
        .unwrap_or_else(|| panic!("JSON path {keys:?} is not a string"))
        .as_str()
}

/// Render a JSON node back to its compact string form (for comparing the cty
/// type constraints the engine reports).
pub fn to_json_string(value: &Json) -> String {
    facet_json::to_string(value).expect("value re-serializes")
}
