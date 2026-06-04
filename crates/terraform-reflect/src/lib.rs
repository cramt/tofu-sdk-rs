//! Reflection: Rust types -> provider semantic IR.
//!
//! This crate is the bridge from facet's reflection graph to [`terraform_ir`].
//! It walks a [`facet::Shape`] and reads the `#[facet(terraform::...)]`
//! extension attributes (declared in `terraform-attrs`) to produce a
//! [`terraform_ir::Block`].
//!
//! The attributes are read positionally by string key
//! (`field.get_attr(Some("terraform"), "required")`), so this crate does not
//! depend on `terraform-attrs` at all for the read path — only the tests do, to
//! exercise real derived types.

mod reader;

pub use reader::{reflect_block, reflect_data_source, reflect_resource, ReflectError};

#[cfg(test)]
mod spike_tests {
    //! Minimal proof that facet's extension-attribute system carries our
    //! `terraform::*` metadata and that we can read it back at runtime. If this
    //! fails, the whole reflection strategy is invalid and everything downstream
    //! needs rethinking — so it is the first test.

    use facet::Facet;
    use terraform_attrs as terraform;

    #[derive(Facet)]
    #[facet(terraform::resource)]
    #[allow(dead_code)]
    struct Bucket {
        #[facet(terraform::required)]
        #[facet(terraform::force_new)]
        name: String,

        #[facet(terraform::computed)]
        arn: String,
    }

    fn fields() -> &'static [facet::Field] {
        match &Bucket::SHAPE.ty {
            facet::Type::User(facet::UserType::Struct(s)) => s.fields,
            _ => panic!("Bucket should reflect as a struct"),
        }
    }

    #[test]
    fn container_attr_round_trips() {
        let present = Bucket::SHAPE
            .attributes
            .iter()
            .any(|a| a.ns == Some("terraform") && a.key == "resource");
        assert!(
            present,
            "Bucket should carry the terraform::resource marker"
        );
    }

    #[test]
    fn field_attrs_round_trip() {
        let fields = fields();

        let name = fields.iter().find(|f| f.name == "name").unwrap();
        assert!(
            name.has_attr(Some("terraform"), "required"),
            "name should be terraform::required"
        );
        assert!(
            name.has_attr(Some("terraform"), "force_new"),
            "name should be terraform::force_new"
        );
        assert!(
            !name.has_attr(Some("terraform"), "computed"),
            "name should not be computed"
        );

        let arn = fields.iter().find(|f| f.name == "arn").unwrap();
        assert!(
            arn.has_attr(Some("terraform"), "computed"),
            "arn should be terraform::computed"
        );
        assert!(
            !arn.has_attr(Some("terraform"), "required"),
            "arn should not be required"
        );
    }
}
