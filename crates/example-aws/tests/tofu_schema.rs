//! Contract test: drive the **real** OpenTofu/Terraform binary against the
//! example provider and assert on the schema it reports.
//!
//! OpenTofu launches our plugin exactly as in production — go-plugin handshake,
//! auto-mTLS, `GetProviderSchema` — and we assert on the decoded JSON. If this
//! passes, the provider is genuinely Terraform-compatible.

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
"#;

#[test]
fn real_engine_loads_reflected_schema() {
    let engine = common::engine();
    let ws = common::workspace(CONFIG);

    let output = common::run(&engine, &["providers", "schema", "-json"], &ws);
    common::assert_ok(&format!("{engine} providers schema -json"), &output);

    let schema = common::json(&output.stdout);

    // The provider key is host-qualified (registry.opentofu.org or
    // registry.terraform.io); match by suffix instead of hardcoding the host.
    let providers = common::get(&schema, &["provider_schemas"])
        .as_object()
        .expect("provider_schemas object");
    let (_, provider) = providers
        .iter()
        .find(|(k, _)| k.as_str().ends_with("example/aws"))
        .expect("our provider is present");

    let attrs = common::get(
        provider,
        &["resource_schemas", "aws_s3_bucket", "block", "attributes"],
    );

    assert_eq!(
        common::to_json_string(common::get(attrs, &["name", "type"])),
        r#""string""#
    );
    assert_eq!(
        common::get(attrs, &["name", "required"]).as_bool(),
        Some(true)
    );
    assert_eq!(
        common::to_json_string(common::get(attrs, &["arn", "type"])),
        r#""string""#
    );
    assert_eq!(
        common::get(attrs, &["arn", "computed"]).as_bool(),
        Some(true)
    );
    assert_eq!(
        common::to_json_string(common::get(attrs, &["tags", "type"])),
        r#"["map","string"]"#,
        "tags should be map(string)"
    );

    assert!(
        common::path(provider, &["provider", "block"])
            .and_then(Json::as_object)
            .is_some(),
        "provider schema block must be present"
    );

    // The provider-defined function `arn_for(name) -> string`.
    let func = common::get(provider, &["functions", "arn_for"]);
    assert_eq!(
        common::to_json_string(common::get(func, &["return_type"])),
        r#""string""#,
        "arn_for returns a string"
    );
    let params = common::get(func, &["parameters"])
        .as_array()
        .expect("arn_for parameters array");
    assert_eq!(params.len(), 1, "arn_for takes one parameter");
    assert_eq!(
        common::to_json_string(common::get(&params[0], &["type"])),
        r#""string""#,
        "the `name` parameter is a string"
    );
}
