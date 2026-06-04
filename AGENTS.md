# Agent / contributor guide

Working notes for anyone (human or agent) hacking on `tofu-sdk-rs`. For the
user-facing pitch and example, see [README.md](README.md).

## What this is

A clean-room Rust SDK for Terraform/OpenTofu providers. The guiding rule:

```text
Rust types → facet reflection → provider IR → Terraform tfplugin6 backend
```

The **provider IR** (`terraform-ir`) is the stable, backend-agnostic contract.
Terraform is just one emitter; keep Terraform/protocol specifics out of the IR
and the public API.

## Environment

This repo uses a Nix flake dev shell that pins the whole toolchain. **Run every
command inside it:**

```bash
nix develop --command bash -c 'cargo test --workspace'
# or, interactively:
nix develop
```

It provides Rust 1.96, `protoc` (with `PROTOC` set, for the gRPC codegen),
OpenTofu 1.12 (`tofu`), and cargo extras (`nextest`, `expand`, `llvm-cov`).
`.envrc` (`use flake`) auto-loads it with direnv.

Before committing, keep it green:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets   # must be warning-free
cargo test --workspace
```

## Crate layout & dependency direction

```
terraform-value     cty Type, dynamic Value tree, TfValue (no deps)
terraform-ir        provider IR  (depends on value only)
terraform-attrs     the `terraform` facet attribute namespace (its own crate, see below)
terraform-reflect   facet::Shape -> IR
terraform-codec     cty DynamicValue codec (msgpack/JSON) + typed encode/decode (facet Peek/Partial)
terraform-tfplugin6 vendored proto + generated gRPC + IR -> Terraform schema emitter
terraform-runtime   Resource trait, gRPC service impl, planning, handshake/serve
terraform-provider  public author-facing facade (re-exports)
terraform-macros    reserved (empty)
example-aws         example provider binary + the real-tofu contract tests
```

Keep `terraform-ir` free of Terraform protocol concerns. All Terraform-specific
mapping lives in `terraform-tfplugin6` (the "backend").

## Conventions & gotchas (read before changing these)

- **facet comes from crates.io** (`facet = "0.46"`, resolving to 0.46.5).
  `facet-reflect 0.46.5` is now published, so the `reflect` feature
  (Peek/Partial) resolves without a git source or a `[patch.crates-io]`
  override. (Historically we pinned the git repo because `facet-reflect 0.46.5`
  was unpublished — that's fixed.)
- **No serde — JSON is `facet-json` + `facet-value`.** All JSON (the cty
  type-constraint encoding and cty JSON state) goes through `facet-json`
  (typed (de)serialize) and `facet-value` (its dynamic `Value`). These live in
  the `facet-format` repo (split out of the facet monorepo); we take them from
  crates.io (`= "0.46"`). Their transitive facet-* deps are `^0.46` and unify
  on facet-core 0.46.5 alongside the main `facet` crate. cty<->JSON lives on
  `terraform_value::Type` (`to_cty_json_bytes` / `from_cty_json_bytes`).
- **`terraform-attrs` is a separate crate on purpose.** A facet
  `define_attr_grammar!` emits a `#[macro_export]` dispatcher that cannot be
  used by path within its own crate (rust-lang/rust#52234). Authors alias it as
  `terraform` (re-exported via `terraform-provider`).
- **`reflect` feature**: `terraform-codec` enables facet's `reflect` feature for
  `Peek`/`Partial`.
- **`force_new` is a plan behavior, not a schema property** — it is reflected
  but emitted as `requires_replace` during `PlanResourceChange`, never into the
  schema.
- **Decode of `Unknown`/null-on-non-`Option` → the type's zero value.** Plain
  Rust types can't hold "unknown"; resource handlers fill computed fields, so
  this is fine in practice. A `TfValue<T>` wrapper to preserve the distinction
  is a future refinement.
- **Numbers are `f64`** in the `Value` tree (lossy for very large/precise
  numbers; fine for real configs).
- **auto-mTLS is server-auth-only.** tonic's `client_ca_root` is
  go-plugin-incompatible (advertises CA-name hints; the Go client then withholds
  its cert). We terminate TLS ourselves (tokio-rustls), present + advertise a
  self-signed CA cert that the host pins, and do not require a client cert. See
  `terraform-runtime/src/tls.rs`.
- **Do not watch stdin for shutdown** — non-interactive launches inherit a
  closed stdin and would exit before the host connects. Shutdown is SIGTERM /
  Ctrl-C only (`serve.rs`).
- **`GetProviderSchema` must always include a provider block** (empty if the
  provider takes no config) or Terraform errors "missing provider schema".

## How a resource works

Authors implement the async `Resource` trait over a `#[derive(Facet)]` `Model`
(`create` required; `read`/`update`/`delete` have defaults). The runtime wraps
each handler in an erased `DynResource` (`resource.rs`) that decodes the dynamic
`Value` into the model, calls the typed method, and encodes the result back. The
gRPC service (`service.rs`) dispatches by type name and drives the codec; the
planning engine lives in `plan.rs`.

Data sources mirror this exactly: the read-only `DataSource` trait (`read`) over
a `Model`, erased as `DynDataSource` (`data_source.rs`), dispatched on
`ReadDataSource`. Register eagerly with `ProviderBuilder::data_source` or
meta-backed with `data_source_with` (same `Arc<Meta>` wiring as
`resource`/`resource_with`). Resources and data sources share a type-name
namespace per provider — `resource "aws_s3_bucket"` and `data "aws_s3_bucket"`
can coexist (separate maps in the IR `ProviderSchema`).

## Testing approach

Three layers, deliberately:

1. **Logic via direct trait calls** — the generated gRPC service is an ordinary
   async trait, so tests construct `ProviderService` and call methods directly
   (no socket/client). See `terraform-runtime/tests/service.rs`.
2. **Native `tofu test` e2e suite** — the lifecycle is driven by the engine's
   own test framework. The `.tftest.hcl` files in `example-aws/tests/tofu/`
   hold the real `run`/`assert` blocks (apply/plan, computed values, provider
   config, `force_new` replacement); `tofu test` performs real apply/destroy
   cycles through the plugin protocol. `example-aws/tests/tofu_test.rs` is a
   thin runner that lays out the `dev_overrides` workspace and shells out to
   `tofu test` so the suite runs under `cargo test --workspace`.
3. **Schema contract test** — `example-aws/tests/tofu_schema.rs` parses
   `providers schema -json` (the native test framework only asserts plan/apply
   state, not schema, so this stays a Rust test).

Both engine-backed layers **require `tofu` or `terraform` on `PATH`** (the dev
shell provides it) and are the source of truth for protocol compatibility. The
shared `dev_overrides` workspace plumbing lives in `tests/common/mod.rs`.

Notes / gotchas for the `tofu test` suite:
- **`force_new` is asserted indirectly via `last_action`.** The framework can
  assert attribute *values* but not the planned *action* (replace vs in-place
  update). The example `Bucket` has a computed `last_action` set to `"created"`
  by `create` and `"updated"` by `update`; a replacement re-runs `create`, so
  asserting `last_action == "created"` after a rename proves replacement.
- **Don't add an in-place run that expects `last_action == "updated"`.** The
  planning engine only marks computed attrs unknown when null or replacing
  (`plan.rs`), so an in-place update that changed `last_action` would trip
  Terraform's "inconsistent result after apply" check.

Do not reintroduce hand-rolled gRPC clients / subprocess+UDS plumbing for tests
— the engine-backed path is both simpler and higher-fidelity.

### Run the example against tofu by hand

```bash
cargo build -p example-aws
DIR=$(mktemp -d); ln -s "$PWD/target/debug/example-aws" "$DIR/terraform-provider-aws"
cat > "$DIR/tofurc" <<EOF
provider_installation {
  dev_overrides { "example/aws" = "$DIR" }
  direct {}
}
EOF
printf 'terraform { required_providers { aws = { source = "example/aws" } } }\n' > "$DIR/main.tf"
(cd "$DIR" && TF_CLI_CONFIG_FILE="$DIR/tofurc" tofu providers schema -json | jq .)
```

## Status

The 5-phase MVP is complete and verified against real OpenTofu. See the README
"Not yet implemented" section for what is intentionally missing.
