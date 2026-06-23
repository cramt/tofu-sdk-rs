//! Contract tests for the protocol surfaces OpenTofu 1.12 can't drive but
//! HashiCorp Terraform 1.15 can: **list resources** (`terraform query`),
//! **state stores**, and **resource identity** — all of which OpenTofu 1.12 drops
//! from `providers schema -json`. These drive the real `terraform` binary against
//! the example provider through the same `dev_overrides` workspace as the OpenTofu
//! suite. They require Terraform >= 1.14 on PATH (the Nix dev shell provides 1.15);
//! when it is absent the tests no-op with a skip notice.

mod common;

use std::fs;

const CONFIG: &str = r#"
terraform {
  required_providers {
    aws = {
      source = "example/aws"
    }
  }
}

provider "aws" {}
"#;

/// `providers schema -json` from Terraform 1.15 surfaces the list-resource,
/// state-store, and resource-identity schemas (the three things OpenTofu 1.12
/// drops), so this is the schema-contract layer for those primitives.
#[test]
fn terraform_surfaces_list_state_and_identity_schemas() {
    let Some(engine) = common::terraform() else {
        eprintln!("skipping: Terraform >= 1.14 not on PATH");
        return;
    };
    let ws = common::workspace(CONFIG);

    let output = common::run(&engine, &["providers", "schema", "-json"], &ws);
    common::assert_ok("terraform providers schema -json", &output);
    let schema = common::json(&output.stdout);

    let providers = common::get(&schema, &["provider_schemas"])
        .as_object()
        .expect("provider_schemas object");
    let (_, provider) = providers
        .iter()
        .find(|(k, _)| k.as_str().ends_with("example/aws"))
        .expect("our provider is present");

    // The list resource publishes its `list {}` query block (the name-prefix
    // filter), distinct from the managed resource's own attributes.
    assert_eq!(
        common::get(
            provider,
            &[
                "list_resource_schemas",
                "aws_locker",
                "block",
                "attributes",
                "name_prefix",
                "optional",
            ],
        )
        .as_bool(),
        Some(true),
        "the list resource exposes its optional `name_prefix` query input",
    );

    // The state store publishes its config block.
    assert!(
        common::path(provider, &["state_store_schemas", "inmem", "block"])
            .and_then(common::Json::as_object)
            .is_some(),
        "the `inmem` state store is published in state_store_schemas",
    );

    // The resource identity is published under the shared identity-schema map
    // (the list resource projects each result to the managed resource's identity).
    assert_eq!(
        common::get(
            provider,
            &[
                "resource_identity_schemas",
                "aws_locker",
                "attributes",
                "name",
                "required_for_import",
            ],
        )
        .as_bool(),
        Some(true),
        "the `name` identity attribute is required for import",
    );
}

/// `terraform query` drives the `aws_locker` list resource end-to-end: the engine
/// calls `ValidateListResourceConfig` then `ListResource`, and the handler's
/// results stream back projected to their `name` identity.
#[test]
fn terraform_query_lists_existing_instances() {
    let Some(engine) = common::terraform() else {
        eprintln!("skipping: Terraform >= 1.14 not on PATH");
        return;
    };
    let ws = common::workspace(CONFIG);
    // A `.tfquery.hcl` file with a `list {}` block drives the query. With no
    // prefix filter the handler returns all three synthesized lockers, each
    // projected to its `name` identity.
    fs::write(
        ws.cfg.join("list.tfquery.hcl"),
        r#"
list "aws_locker" "all" {
  provider = aws
  config {}
}
"#,
    )
    .unwrap();

    let output = common::run(&engine, &["query"], &ws);
    common::assert_ok("terraform query", &output);
    let stdout = String::from_utf8_lossy(&output.stdout);

    for name in ["alpha", "beta", "gamma"] {
        assert!(
            stdout.contains(&format!("name={name}")),
            "expected listed locker `{name}` (with its `name` identity) in query output:\n{stdout}",
        );
    }
}

/// The action surfaces in `providers schema -json` under `action_schemas`.
#[test]
fn terraform_surfaces_action_schema() {
    let Some(engine) = common::terraform() else {
        eprintln!("skipping: Terraform >= 1.14 not on PATH");
        return;
    };
    let ws = common::workspace(CONFIG);

    let output = common::run(&engine, &["providers", "schema", "-json"], &ws);
    common::assert_ok("terraform providers schema -json", &output);
    let schema = common::json(&output.stdout);
    let (_, provider) = common::get(&schema, &["provider_schemas"])
        .as_object()
        .expect("provider_schemas object")
        .iter()
        .find(|(k, _)| k.as_str().ends_with("example/aws"))
        .expect("our provider is present");

    assert_eq!(
        common::get(
            provider,
            &[
                "action_schemas",
                "aws_publish",
                "block",
                "attributes",
                "topic",
                "required",
            ],
        )
        .as_bool(),
        Some(true),
        "the action's `topic` input is published as required",
    );
}

/// `terraform apply` triggers the action after a resource is created
/// (`action_trigger`), running `PlanAction` then the streaming `InvokeAction`:
/// the progress messages the handler emits via `ctx.progress` reach the output.
#[test]
fn terraform_apply_invokes_a_triggered_action() {
    let Some(engine) = common::terraform() else {
        eprintln!("skipping: Terraform >= 1.14 not on PATH");
        return;
    };
    let ws = common::workspace(
        r#"
terraform {
  required_providers {
    aws = {
      source = "example/aws"
    }
  }
}

provider "aws" {
  region = "eu-west-1"
}

action "aws_publish" "notify" {
  config {
    topic   = "deploys"
    message = "hello-world"
  }
}

resource "aws_s3_bucket" "b" {
  name = "trigger-bucket"
  tags = { env = "test" }
  lifecycle {
    action_trigger {
      events  = [after_create]
      actions = [action.aws_publish.notify]
    }
  }
}
"#,
    );

    let apply = common::run(&engine, &["apply", "-auto-approve"], &ws);
    common::assert_ok("terraform apply", &apply);
    let stdout = String::from_utf8_lossy(&apply.stdout);
    // Both progress messages the action streamed via `ctx.progress` appear, and
    // the invocation completed.
    assert!(
        stdout.contains("publishing to deploys"),
        "the action's first progress message reached the output:\n{stdout}",
    );
    assert!(
        stdout.contains("published: hello-world"),
        "the action's second progress message reached the output:\n{stdout}",
    );

    common::run(&engine, &["destroy", "-auto-approve"], &ws);
}
