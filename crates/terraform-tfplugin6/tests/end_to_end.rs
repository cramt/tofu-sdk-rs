//! Phase 1 vertical slice: Rust type -> facet reflection -> provider IR ->
//! Terraform `tfplugin6` schema, asserted end to end.
//!
//! This is the proof that the whole pipeline composes:
//!
//! ```text
//! #[derive(Facet)] struct  ->  reflect_resource  ->  ResourceSchema (IR)
//!                                                 ->  emit_schema    ->  tfplugin6::Schema
//! ```

use std::collections::HashMap;

use facet::Facet;
use terraform_attrs as terraform;
use terraform_reflect::reflect_resource;
use terraform_tfplugin6::{emit_schema, tfplugin6};

#[derive(Facet)]
#[facet(terraform::resource)]
#[allow(dead_code)]
struct Bucket {
    /// The name of the bucket.
    #[facet(terraform::force_new)]
    name: String,

    /// The ARN assigned by AWS.
    #[facet(terraform::computed)]
    arn: String,

    /// Free-form tags.
    tags: HashMap<String, String>,

    /// Object versions retained.
    versions: Vec<String>,

    /// Days to retain objects before expiry.
    retention_days: Option<i64>,

    /// Whether server-side encryption is enabled.
    encrypted: bool,
}

/// The `cty` JSON type constraint stored in an attribute's `type` bytes, as a
/// JSON string.
fn cty(attr: &tfplugin6::schema::Attribute) -> String {
    String::from_utf8(attr.r#type.clone()).expect("attribute type is valid UTF-8 JSON")
}

fn attr<'a>(block: &'a tfplugin6::schema::Block, name: &str) -> &'a tfplugin6::schema::Attribute {
    block
        .attributes
        .iter()
        .find(|a| a.name == name)
        .unwrap_or_else(|| panic!("missing attribute `{name}`"))
}

#[test]
fn bucket_reflects_to_terraform_schema() {
    // Reflect the Rust type into the provider IR.
    let resource = reflect_resource::<Bucket>("aws_s3_bucket").expect("Bucket reflects");
    assert_eq!(resource.name, "aws_s3_bucket");

    // Emit the IR to a Terraform protocol schema at schema version 3.
    let schema = emit_schema(&resource.block, 3);
    assert_eq!(schema.version, 3);
    let block = schema.block.expect("schema has a top-level block");

    // name: required, force-new (force_new is NOT a schema property), string.
    let name = attr(&block, "name");
    assert!(name.required);
    assert!(!name.optional);
    assert!(!name.computed);
    assert_eq!(cty(name), r#""string""#);
    assert_eq!(name.description, "The name of the bucket.");

    // arn: computed-only.
    let arn = attr(&block, "arn");
    assert!(arn.computed);
    assert!(!arn.required);
    assert!(!arn.optional);
    assert_eq!(cty(arn), r#""string""#);

    // tags: required map(string) (no explicit disposition, non-Option -> required).
    let tags = attr(&block, "tags");
    assert_eq!(cty(tags), r#"["map","string"]"#);
    assert!(tags.required);

    // versions: list(string).
    assert_eq!(cty(attr(&block, "versions")), r#"["list","string"]"#);

    // retention_days: Option<i64> -> optional number.
    let retention = attr(&block, "retention_days");
    assert_eq!(cty(retention), r#""number""#);
    assert!(retention.optional);
    assert!(!retention.required);

    // encrypted: bool.
    assert_eq!(cty(attr(&block, "encrypted")), r#""bool""#);

    // Every attribute must satisfy Terraform's rule: at least one of
    // required/optional/computed must be set.
    for a in &block.attributes {
        assert!(
            a.required || a.optional || a.computed,
            "attribute `{}` has no disposition",
            a.name
        );
    }
}
