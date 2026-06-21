//! Shared helpers for the PostgreSQL contract tests.
//!
//! Two moving parts:
//!  1. [`Postgres`] — a throwaway PostgreSQL server in Docker. It publishes the
//!     container's 5432 to a random localhost port, waits until the server
//!     actually answers queries, and force-removes the container on drop.
//!  2. [`Workspace`] — a `dev_overrides` layout pointing OpenTofu at the freshly
//!     built provider binary, with the `PG*` environment wired to the container
//!     so the provider connects to it (no provider block needed).
//!
//! Docker is **required**: these tests panic (rather than skip) if it is absent,
//! matching how the suite already requires `tofu`/`terraform`.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IMAGE: &str = "postgres:16-alpine";
const SUPERUSER: &str = "postgres";
const PASSWORD: &str = "postgres";
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// Locate the CLI binary, preferring OpenTofu. Panics if neither is installed.
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

/// Assert Docker is available, panicking with guidance otherwise.
fn require_docker() {
    let ok = Command::new("docker")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        ok,
        "these contract tests require a working Docker daemon on PATH \
         (they spin up `{IMAGE}` to run a real `tofu test`)"
    );
}

/// A disposable PostgreSQL container.
pub struct Postgres {
    container_id: String,
    pub host: String,
    pub port: u16,
}

impl Postgres {
    /// Start the container, publish a random localhost port, and block until the
    /// server answers a trivial query.
    pub fn start() -> Self {
        require_docker();

        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "-e",
                &format!("POSTGRES_PASSWORD={PASSWORD}"),
                "-p",
                "127.0.0.1::5432/tcp",
                IMAGE,
            ])
            .output()
            .expect("run docker");
        assert!(
            out.status.success(),
            "failed to start postgres container:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let container_id = String::from_utf8_lossy(&out.stdout).trim().to_string();

        let pg = Self {
            port: published_port(&container_id),
            host: "127.0.0.1".to_string(),
            container_id,
        };
        pg.wait_ready();
        pg
    }

    /// Poll until `SELECT 1` succeeds inside the container.
    fn wait_ready(&self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let out = Command::new("docker")
                .args([
                    "exec",
                    &self.container_id,
                    "psql",
                    "-U",
                    SUPERUSER,
                    "-d",
                    SUPERUSER,
                    "-tAc",
                    "SELECT 1",
                ])
                .output();
            if let Ok(out) = out {
                if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "1" {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "postgres container did not become ready within {READY_TIMEOUT:?}"
            );
            sleep(Duration::from_millis(500));
        }
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container_id])
            .output();
    }
}

/// Read the host port Docker mapped to the container's 5432.
fn published_port(container_id: &str) -> u16 {
    let out = Command::new("docker")
        .args(["port", container_id, "5432/tcp"])
        .output()
        .expect("docker port");
    assert!(out.status.success(), "docker port failed");
    // e.g. "127.0.0.1:49153" (possibly multiple lines for v4/v6).
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|line| {
            line.rsplit_once(':')
                .and_then(|(_, p)| p.trim().parse().ok())
        })
        .expect("parse published port")
}

/// A self-cleaning temp directory.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tofu-sdk-rs-pg-it-{}-{}",
            std::process::id(),
            nanos
        ));
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

/// A `dev_overrides` workspace pointing at the built provider.
pub struct Workspace {
    _dir: TempDir,
    pub tofurc: PathBuf,
    pub cfg: PathBuf,
}

fn empty_workspace() -> Workspace {
    let provider_bin = env!("CARGO_BIN_EXE_terraform-provider-postgres");
    let dir = TempDir::new();

    let plugins = dir.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    symlink(provider_bin, plugins.join("terraform-provider-postgres")).expect("symlink provider");

    let tofurc = dir.path().join("tofurc");
    fs::write(
        &tofurc,
        format!(
            "provider_installation {{\n  dev_overrides {{\n    \"example/postgres\" = {plugins:?}\n  }}\n  direct {{}}\n}}\n"
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

/// Build a workspace by copying every file from `fixtures_dir` into the config
/// dir (the `.tf` config plus the `.tftest.hcl` suite).
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

/// Run an engine command in the workspace, wiring the `PG*` environment at the
/// container so the provider connects to it.
pub fn run(engine: &str, args: &[&str], ws: &Workspace, pg: &Postgres) -> Output {
    Command::new(engine)
        .args(args)
        .current_dir(&ws.cfg)
        .env("TF_CLI_CONFIG_FILE", &ws.tofurc)
        .env("PGHOST", &pg.host)
        .env("PGPORT", pg.port.to_string())
        .env("PGUSER", SUPERUSER)
        .env("PGPASSWORD", PASSWORD)
        .env("PGDATABASE", SUPERUSER)
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
