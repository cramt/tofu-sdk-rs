# tofu-sdk-rs

A clean-room Rust SDK for building [Terraform](https://www.terraform.io/) /
[OpenTofu](https://opentofu.org/) providers, driven by **reflection over plain
Rust types** rather than hand-written schemas.

```text
Rust types
    │  #[derive(Facet)] + #[facet(terraform::…)]
    ▼
Facet reflection graph
    ▼
Provider semantic IR        ← the stable, backend-agnostic contract
    ▼
Terraform tfplugin6 backend ← just one emitter; TS/Ruby/WASM could be others
```

Terraform is **not** the source of truth. Your Rust types are. The Terraform
plugin protocol is a backend the SDK emits *to*, which keeps the door open for
other backends later and keeps the dynamic protocol details out of your code.

## Example

```rust
use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{serve, Provider};

#[derive(Facet)]
#[facet(terraform::resource)]
struct Bucket {
    /// The globally-unique name of the bucket.
    #[facet(terraform::required)]
    #[facet(terraform::force_new)]
    name: String,

    /// The ARN assigned after creation.
    #[facet(terraform::computed)]
    arn: String,

    /// Free-form tags.
    tags: std::collections::HashMap<String, String>,
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .resource::<Bucket>("aws_s3_bucket")
        .build()
        .expect("provider definition is valid");
    serve(provider).await.expect("serve");
}
```

Point OpenTofu at the built binary with a `dev_overrides` CLI config and
`tofu providers schema -json` returns the schema reflected from `Bucket` — no
schema boilerplate, no stringly-typed field plumbing.

## Status

This is an early, in-progress implementation.

- **Phase 1 ✅** — reflection → provider IR → `tfplugin6` schema emission
- **Phase 2 ✅** — `tfplugin6` gRPC server, go-plugin handshake, auto-mTLS,
  `GetProviderSchema` (verified end-to-end against real OpenTofu)
- **Phase 3 ✅** — `cty` `DynamicValue` msgpack codec (known/unknown/null) and
  typed encode (Rust value → dynamic value via reflection); typed decode folds
  into Phase 4
- **Phase 4 🚧** — the `Resource` trait, typed decode, `ConfigureProvider`,
  `ReadResource`, `ApplyResourceChange`
- **Phase 5** — planning engine (replacement semantics, unknown propagation)

## Workspace layout

| Crate | Role |
|-------|------|
| `terraform-value` | The `cty` type system and `TfValue` (known/unknown/null) |
| `terraform-ir` | Backend-agnostic provider IR (the stable internal contract) |
| `terraform-attrs` | The `#[facet(terraform::…)]` attribute namespace |
| `terraform-reflect` | `facet::Shape` → IR |
| `terraform-tfplugin6` | Vendored protocol + IR → Terraform schema emitter + gRPC service |
| `terraform-runtime` | Provider builder, gRPC service impl, handshake/serve |
| `terraform-provider` | The public, author-facing facade |
| `terraform-codec` | DynamicValue codecs (Phase 3) |
| `terraform-macros` | Reserved for convenience derives |
| `example-aws` | A minimal example provider + the OpenTofu contract test |

## Developing

A Nix flake provides the full toolchain (Rust, `protoc`, OpenTofu):

```bash
nix develop
cargo test --workspace
```

The contract test in `example-aws` drives a real `tofu`/`terraform` binary, so
it requires one on `PATH` (provided by the dev shell).

## License

MIT OR Apache-2.0
