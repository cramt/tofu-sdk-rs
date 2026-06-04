//! Contract test: drive the **real** OpenTofu/Terraform binary against the
//! example provider and assert on the schema it reports.
//!
//! OpenTofu launches our plugin exactly as in production — go-plugin handshake,
//! auto-mTLS, `GetProviderSchema` — and we assert on the decoded JSON. If this
//! passes, the provider is genuinely Terraform-compatible.

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
"#;

#[test]
fn real_engine_loads_reflected_schema() {
    let engine = common::engine();
    let ws = common::workspace(CONFIG);

    let output = common::run(&engine, &["providers", "schema", "-json"], &ws);
    common::assert_ok(&format!("{engine} providers schema -json"), &output);

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

    assert!(
        provider["provider"]["block"].is_object(),
        "provider schema block must be present"
    );
}
