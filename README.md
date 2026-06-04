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
use terraform_runtime::{async_trait, serve, Provider, Resource, ResourceError};

/// The resource's schema, reflected from a plain Rust struct.
#[derive(Facet)]
#[facet(terraform::resource)]
struct Bucket {
    #[facet(terraform::required)]
    #[facet(terraform::force_new)]
    name: String,

    /// Computed: derived from `name` during create.
    #[facet(terraform::computed)]
    arn: String,
}

struct BucketResource;

#[async_trait]
impl Resource for BucketResource {
    type Model = Bucket;

    async fn create(&self, mut planned: Bucket) -> Result<Bucket, ResourceError> {
        planned.arn = format!("arn:aws:s3:::{}", planned.name);
        Ok(planned)
    }
    // read defaults to a passthrough, delete to a no-op.
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .resource("aws_s3_bucket", BucketResource)
        .build()
        .expect("provider definition is valid");
    serve(provider).await.expect("serve");
}
```

Point OpenTofu at the built binary with a `dev_overrides` CLI config and
`tofu apply` creates the resource — the schema is reflected from `Bucket`, and
the planned/prior state is decoded into `Bucket` and back automatically. No
schema boilerplate, no stringly-typed field plumbing.

## Status

The original 5-phase MVP is complete: a plain Rust struct plus a `Resource`
impl is a working provider, exercised end-to-end against **real OpenTofu**
(`apply` / `plan` / `destroy` / replacement). It is not yet production-hardened.

- **Phase 1 ✅** — reflection → provider IR → `tfplugin6` schema emission
- **Phase 2 ✅** — `tfplugin6` gRPC server, go-plugin handshake, auto-mTLS,
  `GetProviderSchema` (verified end-to-end against real OpenTofu)
- **Phase 3 ✅** — `cty` `DynamicValue` codec (msgpack + JSON, known/unknown/null)
  and typed encode/decode between Rust values and the dynamic value tree
- **Phase 4 ✅** — the `Resource` trait (create/read/update/delete) and the full
  lifecycle (`ConfigureProvider`, validation, `UpgradeResourceState`,
  `PlanResourceChange`, `ReadResource`, `ApplyResourceChange`) — verified by a
  real `tofu apply`/`destroy` test
- **Phase 5 ✅** — planning engine: changing a `force_new` attribute emits
  `requires_replace` (destroy + create), computed attributes go unknown on
  replacement — verified by a real `tofu` replacement test
- **Provider configuration ✅** — `ConfigureProvider` decodes the provider
  config block and a `configure` closure turns it into shared state (an
  `Arc<Meta>`, e.g. an API client) handed to every resource handler — verified
  by a real `tofu` test that flows a `provider` block region into a resource
- **Data sources ✅** — a read-only `DataSource` trait (`read`) reflected from a
  `Model` and dispatched on `ReadDataSource`; registered eagerly
  (`data_source`) or meta-backed (`data_source_with`) like resources — verified
  by a real `tofu test` `data` block read

### Not yet implemented

Nested blocks end-to-end, functions, ephemeral resources, import/move, and a
`TfValue<T>` field wrapper to preserve known/unknown/null through decode (today
`Unknown` decodes to the type's zero value). Numbers are held as `f64`. Not all
`cty` corner cases are covered.

## Workspace layout

| Crate | Role |
|-------|------|
| `terraform-value` | The `cty` type system, the dynamic `Value` tree, and `TfValue` (known/unknown/null) |
| `terraform-ir` | Backend-agnostic provider IR (the stable internal contract) |
| `terraform-attrs` | The `#[facet(terraform::…)]` attribute namespace |
| `terraform-reflect` | `facet::Shape` → IR |
| `terraform-tfplugin6` | Vendored protocol + IR → Terraform schema emitter + gRPC service |
| `terraform-runtime` | Provider/`Resource` API, gRPC service impl, planning, handshake/serve |
| `terraform-provider` | The public, author-facing facade |
| `terraform-codec` | `DynamicValue` codec (cty msgpack/JSON) + typed encode/decode |
| `terraform-macros` | Reserved for convenience derives |
| `example-aws` | A minimal example provider + the OpenTofu `tofu test` e2e suite |

## Developing

A Nix flake provides the full toolchain (Rust, `protoc`, OpenTofu):

```bash
nix develop
cargo test --workspace
```

The e2e suite in `example-aws` runs the engine's native `tofu test` framework
(`tests/tofu/*.tftest.hcl`) plus a schema contract test, driving a real
`tofu`/`terraform` binary — so it requires one on `PATH` (provided by the dev
shell).

## License

MIT OR Apache-2.0
