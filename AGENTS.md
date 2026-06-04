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

- **facet is a git dependency** (`facet = { git = "..." }` in the root
  `Cargo.toml`). The crates.io `facet` 0.46.5 needs `facet-reflect` 0.46.5,
  which was never published, so the `reflect` feature (Peek/Partial) won't
  resolve from crates.io. Switch back to a crates.io release once that is fixed.
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

## Testing approach

Two layers, deliberately:

1. **Logic via direct trait calls** — the generated gRPC service is an ordinary
   async trait, so tests construct `ProviderService` and call methods directly
   (no socket/client). See `terraform-runtime/tests/service.rs`.
2. **Real-engine contract tests** — `example-aws/tests/` drives an actual
   `tofu`/`terraform` binary via a `dev_overrides` workspace
   (`tests/common/mod.rs`), covering schema, full apply/destroy lifecycle, and
   `force_new` replacement. These **require `tofu` or `terraform` on `PATH`**
   (the dev shell provides it) and are the source of truth for protocol
   compatibility.

Do not reintroduce hand-rolled gRPC clients / subprocess+UDS plumbing for tests
— the real-engine path is both simpler and higher-fidelity.

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
