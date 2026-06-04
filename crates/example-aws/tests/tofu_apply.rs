//! Full-lifecycle contract test: `tofu apply` -> `tofu show` -> `tofu destroy`
//! against the example provider, driven by the real engine.
//!
//! This exercises the whole resource path through real Terraform/OpenTofu:
//! ValidateResourceConfig, PlanResourceChange (computed -> unknown),
//! ApplyResourceChange (create), the cty msgpack codec both ways, then
//! UpgradeResourceState + ReadResource + ApplyResourceChange (delete) on destroy.

mod common;

use common::Json;

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

/// A config whose bucket is named `name`.
fn config(name: &str) -> String {
    format!(
        r#"
terraform {{
  required_providers {{
    aws = {{
      source = "example/aws"
    }}
  }}
}}

resource "aws_s3_bucket" "test" {{
  name = "{name}"
}}
"#
    )
}

/// The single bucket's attribute values from `tofu show -json` output.
fn bucket_values(state: &Json) -> &Json {
    let resources = common::get(state, &["values", "root_module", "resources"])
        .as_array()
        .expect("resources array");
    let bucket = resources
        .as_slice()
        .iter()
        .find(|r| {
            common::path(r, &["type"])
                .and_then(Json::as_string)
                .map(|s| s.as_str())
                == Some("aws_s3_bucket")
        })
        .expect("aws_s3_bucket in state");
    common::get(bucket, &["values"])
}

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
    let state = common::json(&show.stdout);
    let values = bucket_values(&state);

    assert_eq!(common::string(values, &["name"]), "my-bucket");
    assert_eq!(
        common::string(values, &["arn"]),
        "arn:aws:s3:::my-bucket",
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
    let state2 = common::json(&show2.stdout);
    let empty = common::path(&state2, &["values", "root_module", "resources"])
        .and_then(Json::as_array)
        .map(|r| r.is_empty())
        .unwrap_or(true);
    assert!(empty, "all resources destroyed");
}

/// A config with a `provider "aws"` block setting `region`, plus a bucket.
fn config_with_region(region: &str) -> String {
    format!(
        r#"
terraform {{
  required_providers {{
    aws = {{
      source = "example/aws"
    }}
  }}
}}

provider "aws" {{
  region = "{region}"
}}

resource "aws_s3_bucket" "test" {{
  name = "cfg-bucket"
}}
"#
    )
}

#[test]
fn provider_config_region_flows_to_resource() {
    let engine = common::engine();

    // With an explicit provider region, the computed `region` tracks it.
    let ws = common::workspace(&config_with_region("eu-west-1"));
    common::assert_ok(
        "apply (eu-west-1)",
        &common::run(&engine, &["apply", "-auto-approve"], &ws),
    );
    let show = common::run(&engine, &["show", "-json"], &ws);
    common::assert_ok("show (eu-west-1)", &show);
    let state = common::json(&show.stdout);
    assert_eq!(
        common::string(bucket_values(&state), &["region"]),
        "eu-west-1",
        "the configured provider region reaches the resource handler"
    );
    common::assert_ok(
        "destroy (eu-west-1)",
        &common::run(&engine, &["destroy", "-auto-approve"], &ws),
    );

    // With no provider block, `configure` falls back to the default region.
    let ws_default = common::workspace(CONFIG);
    common::assert_ok(
        "apply (default region)",
        &common::run(&engine, &["apply", "-auto-approve"], &ws_default),
    );
    let show = common::run(&engine, &["show", "-json"], &ws_default);
    common::assert_ok("show (default region)", &show);
    let state = common::json(&show.stdout);
    assert_eq!(
        common::string(bucket_values(&state), &["region"]),
        "us-east-1",
        "absent config falls back to the default region"
    );
    common::assert_ok(
        "destroy (default region)",
        &common::run(&engine, &["destroy", "-auto-approve"], &ws_default),
    );
}

#[test]
fn changing_force_new_attribute_replaces() {
    let engine = common::engine();
    let ws = common::workspace(&config("alpha"));

    // Create with name = alpha.
    common::assert_ok(
        "apply (alpha)",
        &common::run(&engine, &["apply", "-auto-approve"], &ws),
    );
    let show = common::run(&engine, &["show", "-json"], &ws);
    common::assert_ok("show (alpha)", &show);
    let state = common::json(&show.stdout);
    assert_eq!(
        common::string(bucket_values(&state), &["arn"]),
        "arn:aws:s3:::alpha"
    );

    // Change the force_new `name` and plan: it must force replacement.
    std::fs::write(ws.cfg.join("main.tf"), config("beta")).unwrap();
    let plan = common::run(&engine, &["plan", "-no-color"], &ws);
    common::assert_ok("plan (beta)", &plan);
    let plan_out = String::from_utf8_lossy(&plan.stdout);
    assert!(
        plan_out.contains("forces replacement"),
        "plan should report that changing `name` forces replacement:\n{plan_out}"
    );

    // Apply the replacement and confirm the computed arn tracks the new name.
    common::assert_ok(
        "apply (beta)",
        &common::run(&engine, &["apply", "-auto-approve"], &ws),
    );
    let show = common::run(&engine, &["show", "-json"], &ws);
    common::assert_ok("show (beta)", &show);
    let state = common::json(&show.stdout);
    assert_eq!(common::string(bucket_values(&state), &["name"]), "beta");
    assert_eq!(
        common::string(bucket_values(&state), &["arn"]),
        "arn:aws:s3:::beta"
    );

    common::assert_ok(
        "destroy",
        &common::run(&engine, &["destroy", "-auto-approve"], &ws),
    );
}
