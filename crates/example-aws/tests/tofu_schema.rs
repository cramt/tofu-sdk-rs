//! Contract test: drive the **real** OpenTofu/Terraform binary against the
//! example provider and assert on the schema it reports.
//!
//! This is the highest-fidelity test in the suite. OpenTofu launches our plugin
//! exactly as in production — performing the go-plugin handshake, auto-mTLS, and
//! the `GetProviderSchema` RPC — and we assert on the JSON it decodes. If this
//! passes, the provider is genuinely Terraform-compatible.
//!
//! It hard-requires a `tofu` (or `terraform`) binary on `PATH` and fails if none
//! is present.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Find the CLI binary, preferring OpenTofu. Panics if neither is installed
/// (this test deliberately requires a real engine).
fn engine() -> String {
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
        "this contract test requires `tofu` or `terraform` on PATH \
         (enter the nix dev shell: `nix develop`)"
    );
}

/// A self-cleaning temp directory.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("tofu-sdk-rs-test-{}-{}", std::process::id(), nanos));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn real_engine_loads_reflected_schema() {
    let engine = engine();
    let provider_bin = env!("CARGO_BIN_EXE_example-aws");

    let work = TempDir::new();
    let plugins = work.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    // dev_overrides expects the binary named `terraform-provider-<type>`.
    symlink(provider_bin, plugins.join("terraform-provider-aws")).expect("symlink provider");

    // CLI config pointing the provider source at our binary directory, skipping
    // installation/init entirely.
    let tofurc = work.path().join("tofurc");
    fs::write(
        &tofurc,
        format!(
            "provider_installation {{\n  dev_overrides {{\n    \"example/aws\" = {dir:?}\n  }}\n  direct {{}}\n}}\n",
            dir = plugins
        ),
    )
    .unwrap();

    // Minimal config that requires the provider.
    let cfg = work.path().join("cfg");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(
        cfg.join("main.tf"),
        "terraform {\n  required_providers {\n    aws = {\n      source = \"example/aws\"\n    }\n  }\n}\n",
    )
    .unwrap();

    let output = Command::new(&engine)
        .args(["providers", "schema", "-json"])
        .current_dir(&cfg)
        .env("TF_CLI_CONFIG_FILE", &tofurc)
        .output()
        .expect("run providers schema");

    assert!(
        output.status.success(),
        "`{engine} providers schema -json` failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let schema: Value = serde_json::from_slice(&output.stdout).expect("schema is valid JSON");

    // The provider key is host-qualified (registry.opentofu.org or
    // registry.terraform.io); match by suffix instead of hardcoding the host.
    let providers = schema["provider_schemas"]
        .as_object()
        .expect("provider_schemas object");
    let (_, provider) = providers
        .iter()
        .find(|(k, _)| k.ends_with("example/aws"))
        .expect("our provider is present");

    let attrs = &provider["resource_schemas"]["aws_s3_bucket"]["block"]["attributes"];

    assert_eq!(attrs["name"]["type"], Value::from("string"));
    assert_eq!(attrs["name"]["required"], Value::from(true));
    assert_eq!(attrs["arn"]["type"], Value::from("string"));
    assert_eq!(attrs["arn"]["computed"], Value::from(true));
    assert_eq!(
        attrs["tags"]["type"],
        serde_json::json!(["map", "string"]),
        "tags should be map(string)"
    );

    // The provider block must be present even though it takes no config.
    assert!(
        provider["provider"]["block"].is_object(),
        "provider schema block must be present"
    );
}
