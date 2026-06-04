//! Generate Rust types and the gRPC `Provider` service for the Terraform plugin
//! protocol v6 from the vendored `tfplugin6.proto`.
//!
//! Both the server (the provider runtime implements it) and the client (handy
//! for in-process integration tests) are generated.
//!
//! `protoc` is provided by the Nix dev shell (the `PROTOC` env var points at
//! it); the well-known `google/protobuf/timestamp.proto` import is resolved from
//! protoc's bundled include directory.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=proto/tfplugin6.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");

    // Make sure protoc's bundled well-known types are on the include path so
    // `import "google/protobuf/timestamp.proto"` resolves regardless of how
    // protoc was packaged.
    let mut includes: Vec<PathBuf> = vec![PathBuf::from("proto")];
    if let Some(dir) = protoc_include_dir() {
        includes.push(dir);
    }

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
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
