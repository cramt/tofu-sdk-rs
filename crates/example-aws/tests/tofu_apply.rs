//! Full-lifecycle contract test: `tofu apply` -> `tofu show` -> `tofu destroy`
//! against the example provider, driven by the real engine.
//!
//! This exercises the whole resource path through real Terraform/OpenTofu:
//! ValidateResourceConfig, PlanResourceChange (computed -> unknown),
//! ApplyResourceChange (create), the cty msgpack codec both ways, then
//! UpgradeResourceState + ReadResource + ApplyResourceChange (delete) on destroy.

mod common;

use serde_json::Value;

const CONFIG: &str = r#"
terraform {
  required_providers {
    aws = {
      source = "example/aws"
    }
  }
}

resource "aws_s3_bucket" "test" {
  name = "my-bucket"
}
"#;

#[test]
fn apply_show_destroy_lifecycle() {
    let engine = common::engine();
    let ws = common::workspace(CONFIG);

    // Create.
    let apply = common::run(&engine, &["apply", "-auto-approve"], &ws);
    common::assert_ok(&format!("{engine} apply"), &apply);

    // Inspect the resulting state (reads the state file; no provider call).
    let show = common::run(&engine, &["show", "-json"], &ws);
    common::assert_ok(&format!("{engine} show -json"), &show);
    let state: Value = serde_json::from_slice(&show.stdout).expect("state json");

    let resources = state["values"]["root_module"]["resources"]
        .as_array()
        .expect("resources array");
    let bucket = resources
        .iter()
        .find(|r| r["type"] == "aws_s3_bucket")
        .expect("aws_s3_bucket in state");
    let values = &bucket["values"];

    assert_eq!(values["name"], Value::from("my-bucket"));
    assert_eq!(
        values["arn"],
        Value::from("arn:aws:s3:::my-bucket"),
        "provider computed the arn during apply"
    );

    // A second plan should report no changes (the computed value is stable).
    let plan = common::run(&engine, &["plan", "-detailed-exitcode"], &ws);
    // -detailed-exitcode: 0 = no changes, 2 = changes, 1 = error.
    assert_ne!(
        plan.status.code(),
        Some(1),
        "second plan errored:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr)
    );
    assert_eq!(
        plan.status.code(),
        Some(0),
        "second plan should show no changes:\nstdout: {}",
        String::from_utf8_lossy(&plan.stdout)
    );

    // Destroy (exercises UpgradeResourceState + ReadResource + delete).
    let destroy = common::run(&engine, &["destroy", "-auto-approve"], &ws);
    common::assert_ok(&format!("{engine} destroy"), &destroy);

    // State should now be empty.
    let show2 = common::run(&engine, &["show", "-json"], &ws);
    common::assert_ok(&format!("{engine} show -json (post-destroy)"), &show2);
    let state2: Value = serde_json::from_slice(&show2.stdout).expect("state json");
    let empty = state2["values"]["root_module"]["resources"]
        .as_array()
        .map(|r| r.is_empty())
        .unwrap_or(true);
    assert!(empty, "all resources destroyed");
}
