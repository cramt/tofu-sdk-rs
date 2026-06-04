//! Generate Rust types for the Terraform plugin protocol v6 from the vendored
//! `tfplugin6.proto`.
//!
//! Phase 1 only needs the message types (for schema emission), so we generate
//! prost messages and skip the gRPC service. Phase 2 will switch to
//! `tonic-prost-build` to additionally generate the `Provider` service.
//!
//! `protoc` is provided by the Nix dev shell (the `PROTOC` env var points at
//! it); the well-known `google/protobuf/timestamp.proto` import is resolved from
//! protoc's bundled include directory.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=proto/tfplugin6.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");

    let mut config = prost_build::Config::new();

    // Make sure protoc's bundled well-known types are on the include path so
    // `import "google/protobuf/timestamp.proto"` resolves regardless of how
    // protoc was packaged.
    let mut includes: Vec<PathBuf> = vec![PathBuf::from("proto")];
    if let Some(dir) = protoc_include_dir() {
        includes.push(dir);
    }

    config
        .compile_protos(&[PathBuf::from("proto/tfplugin6.proto")], &includes)
        .expect("failed to compile tfplugin6.proto");
}

/// Best-effort discovery of protoc's `include` directory (sibling of `bin`),
/// where the well-known type `.proto` files live.
fn protoc_include_dir() -> Option<PathBuf> {
    let protoc = std::env::var_os("PROTOC").map(PathBuf::from)?;
    // <prefix>/bin/protoc -> <prefix>/include
    let include = protoc.parent()?.parent()?.join("include");
    if include.join("google/protobuf/timestamp.proto").exists() {
        Some(include)
    } else {
        None
    }
}
