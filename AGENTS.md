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
terraform-value     cty Type, dynamic Value tree, TfValue (only an optional
                    `facet` feature to derive Facet on TfValue; off by default)
terraform-ir        provider IR  (depends on value only)
terraform-attrs     the `terraform` facet attribute namespace (its own crate, see below)
terraform-reflect   facet::Shape -> IR
terraform-codec     cty DynamicValue codec (msgpack/JSON) + typed encode/decode (facet Peek/Partial)
terraform-tfplugin6 vendored proto + generated gRPC + IR -> Terraform schema emitter
terraform-runtime   Resource trait, gRPC service impl, planning, handshake/serve
terraform-provider  public author-facing facade (re-exports)
terraform-macros    reserved (empty)
example-aws         example provider binary + the real-tofu contract tests
example-fs          side-effecting example provider (writes resource JSON files);
                    subject of the TS iteration-sequence harness

packages/tofu-sdk   @tofu-sdk/core — write providers in TypeScript
  native/           napi-rs Node addon (cdylib) over the dynamic seam
  ts/               typed wrapper compiled to dist/

harness/            TS (Vitest) iteration-sequence harness over example-fs:
                    applies ordered config folders into one shared-state
                    workspace and asserts the JSON files the provider writes
```

Keep `terraform-ir` free of Terraform protocol concerns. All Terraform-specific
mapping lives in `terraform-tfplugin6` (the "backend").

## Frontends & the dynamic seam

The facet path (`terraform-reflect` + the typed `Resource`/`DataSource` traits)
is **one frontend** over a backend-agnostic seam: the IR (`terraform-ir`), the
dynamic `Value` (`terraform-value`), the JSON/msgpack codec (`terraform-codec`,
incl. `encode_json`/`decode_json`), and the erased handler traits
(`DynResource`/`DynDataSource`/`DynConfigure`). Nothing below the erasure ever
sees a facet-derived user type — `plan.rs` and `service.rs` operate purely on IR
+ `Value`.

`ProviderBuilder` exposes that seam directly: `dyn_resource` / `dyn_data_source`
(hand-built `Block` + erased handler) and `dyn_provider_config` / `dyn_configure`.
The **Node binding** (`packages/tofu-sdk/native`) is built entirely on it — it
builds the IR from a JS schema description and implements the erased traits by
calling async JS handlers over `ThreadsafeFunction<String, Promise<String>>`,
marshalling `Value` ⇄ JSON through facet (never hand-rolled). All schema shaping
(singular/plural data sources, search keys) stays in JS; Rust stays
schema-agnostic. Build/test it with `pnpm build` / `pnpm test` inside the dev
shell (it shells out to `cargo`, which needs `PROTOC`); `pnpm test` drives a real
`tofu` through `examples/aws-provider.cjs`.

Schemas in the TS layer (`ts/index.ts`) are **Zod** objects: `z.toJSONSchema` →
cty (the structural derivation Standard Schema can't provide), `z.infer` gives
the handler types, and `safeParse` validates handler output. The Terraform-only
dispositions Zod can't express (`computed`/`forceNew`/`sensitive`/`blocks`) are
arrays typed as `(keyof z.infer<S>)[]`, so a bad field name is a compile error.
This is entirely TS-side; it compiles down to the same cty-JSON the addon takes.
`blocks` names object/array-of-object fields to emit as nested **blocks** (the
addon's `block_from_schema_json` now parses a `blocks` array into IR
`NestedBlock`s; the TS `blockFromField` derives them from the Zod element). So
the TS frontend gets HCL `name { … }` blocks without the facet `terraform::block`
marker — see `examples/cloudflare-provider.ts`.

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
- **Resource type names come from the model, not the builder.**
  `ProviderBuilder::resource` / `resource_with` take no name; it is resolved by
  `terraform_reflect::resource_name::<Model>()` — an explicit
  `#[facet(terraform::resource("aws_s3_bucket"))]`, else `snake_case` of the
  struct identifier. Because `resource("…")` now carries a payload, it is a
  *struct-payload* attribute (like `search_key`), so any crate that writes the
  named form must depend on `terraform-attrs` directly (bare
  `#[facet(terraform::resource)]` still works through the re-export alone).
  Data sources work the same way: `data_source` / `data_source_with` resolve the
  name via `data_source_name` (explicit `#[facet(terraform::data_source("name"))]`
  or `snake_case`), and the plural `data_source_list` / `_with` append an `s`
  (`data_source_list_name`) — so one model backs both a singular `aws_s3_bucket`
  and a plural `aws_s3_buckets`. Irregular plurals aren't handled (the rule is a
  literal `+ "s"`); name a plural-only model's marker so `+ "s"` lands right
  (e.g. `data_source("server")` → `servers`).
- **`reflect` feature**: `terraform-codec` enables facet's `reflect` feature for
  `Peek`/`Partial`.
- **Decoding a `Def::Map` (`HashMap`) uses `begin_key`/`begin_value`, not
  `begin_object_entry`.** The latter is `Def::DynamicValue`-only and errors
  ("begin_object_entry can only be called on DynamicValue types") on a real map.
  See `fill`'s `Def::Map` arm in `terraform-codec/src/typed.rs` (regression test:
  `decodes_map_fields_via_key_value_frames`).
- **`force_new` is a plan behavior, not a schema property** — it is reflected
  but emitted as `requires_replace` during `PlanResourceChange`, never into the
  schema.
- **Nested blocks come only from `#[facet(terraform::block)]`.** A field without
  it that happens to be a struct/`Vec<struct>` stays an *object/list attribute*
  (assigned with `=`); the marker is what makes `terraform-reflect` emit a
  `NestedBlock` (HCL `name { … }`). The IR, the `tfplugin6` emitter, and the
  codec already handle blocks — a block is just an object/list/set/map on the
  wire — so block support lives entirely in `reader.rs::nested_block_from_field`.
  `plan.rs::mark_computed_unknown` now **recurses into nested blocks**, so a
  *computed* attribute inside a block is marked unknown correctly. Required-ness
  of a *single* block is read from the type: a bare struct is a **required**
  single block (`min_items = 1`), an `Option<struct>` is optional
  (`min_items = 0`); collection blocks are always `min_items = 0` (a `NestedBlock`
  now carries `min_items`/`max_items`, emitted by `tfplugin6`). The **singular**
  data-source projection (`reflect_data_source`) keeps a `block` field as a
  read-only nested block (every inner attribute computed, `min_items` 0) instead
  of collapsing it to an object attribute; the **plural** projection still renders
  it as an object attribute inside the computed `results` `list(object(...))`,
  since a repeated HCL block can't be a list element.
- **Decode of `Unknown`/null-on-non-`Option` → the type's zero value.** Plain
  Rust types can't hold "unknown"; resource handlers fill computed fields, so
  this is fine in practice. Use **`TfValue<T>`** (`terraform-value`, re-exported
  as `terraform_provider::TfValue`) for a field that must preserve the
  known/unknown/null distinction through decode — it's special-cased by type
  identifier in `terraform-codec` (`fill_tfvalue`/`tfvalue_to_value`) and
  `terraform-reflect` (`tfvalue_inner`, maps to the inner `T`'s cty type as a
  nullable attribute). The `Facet` derive on `TfValue` is gated behind
  terraform-value's optional `facet` feature (codec/reflect enable it).
- **Attribute defaults** come from `#[facet(terraform::default("…"))]` (a
  struct-payload attr → its consumer crate needs `terraform-attrs` directly). The
  literal is parsed against the attribute's cty type in `reader.rs::field_default`
  and applied by the planner to unset optional attributes; defaults are **not**
  emitted into the schema (Terraform has no schema-level default). Note IR types
  are now `PartialEq` but **not `Eq`** (an `AttributeSchema.default` holds a
  `Value`, hence `f64`).
- **`Resource::modify_plan`** runs after the mechanical plan and returns
  `PlanModifications` (top-level attr names to force-replace / mark unknown). It
  is a defaulted `DynResource` method, so the Node binding and other seam
  implementors need no change.
- **Numbers are `Value::Number(Number)` where `Number` is `I64 | U64 | F64`**
  (`terraform-value`). The full signed+unsigned 64-bit integer range round-trips
  losslessly through msgpack and cty JSON; only truly arbitrary precision (beyond
  64-bit, matching `facet-value`'s own `VNumber` ceiling) is out of reach. The
  codec keeps integers in their exact case; the lossy step is only at the typed
  boundary when an author's declared field type can't hold the value. `Number`
  equality is by mathematical value across cases (`I64(3) == F64(3.0)`), so IR
  types stay `PartialEq` but not `Eq` (a `default` may hold an `F64`).
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
(`create` required; `read`/`update`/`delete` have defaults). Every handler takes
`ctx: &mut Ctx` (`ctx.rs`) — success-path warnings (`ctx.warn`), per-resource
private state (`ctx.private` / `ctx.set_private`), and cancellation
(`ctx.is_cancelled` / `ctx.cancelled`). The runtime wraps each handler in an
erased `DynResource` (`resource.rs`) that decodes the dynamic `Value` into the
model, calls the typed method, and encodes the result back. The gRPC service
(`service.rs`) dispatches by type name and drives the codec; the planning engine
lives in `plan.rs`.

The `Ctx` is **injected ambiently** via a task-local (mirroring the existing
cancellation scope): the service's `run`/`run_diags` install it around the
dispatch and read its outputs back afterwards (`with_ctx`), and the adapter pulls
it with `current_ctx()` to pass to the typed handler. So the **erased
`DynResource`/`DynDataSource` seam is unchanged** — the Node binding and other
dynamic-seam frontends need no change; they just don't see a `Ctx`. A handler
called outside a dispatch (a direct unit test) gets a detached `Ctx` (outputs go
nowhere, never cancelled, no private state).

Data sources mirror this, and can be **projected from the same `Model` as the
resource** (mark the struct `#[facet(terraform::resource)]` *and*
`#[facet(terraform::data_source)]`). Which fields are lookup inputs is driven by
`#[facet(terraform::search_key(exclusive|shared))]`, decoded from a struct
payload (`terraform_attrs::SearchKey`) — the one structured attribute, so a
consumer that uses it needs `terraform-attrs` as a direct dependency (facet's
struct-payload codegen names the crate by path; unit attrs go through the
`terraform_provider::terraform` re-export). The reflection projections live in
`terraform-reflect` (`reflect_data_source` / `reflect_data_source_list`):

- **`search_key(exclusive)`** → unique key, a lookup yields one object. Singular
  data source: the exclusive keys are required inputs, every other field is
  computed. Author implements `DataSource` (`read -> Model`), registered with
  `data_source` / `data_source_with`.
- **`search_key(shared)`** → generic key, a lookup yields many. Plural data
  source: the shared keys are optional inputs plus a computed
  `results = list(object(<model>))`. Author implements `DataSourceList`
  (`list -> Vec<Model>`), registered with `data_source_list` /
  `data_source_list_with`; the adapter assembles the `{inputs…, results}`
  wrapper. A field's data-source role is independent of its resource
  disposition — e.g. a `computed` resource field (an arn) can be the exclusive
  input of its data source.

**Provider-defined functions** (`function.rs`) are pure: an author implements
`Function` over a `Params` struct (fields = ordered positional parameters) and an
`Output` type, registered with `ProviderBuilder::function("name", impl)`. They
need no `configure` (no meta), so they are always eager. `reflect_function`
(`terraform-reflect`) builds the `FunctionSignature` IR — each field maps to a
`Parameter` (name + cty type; `allow_null` from `Option`/`TfValue`), `Output` to
the return type; variadic parameters aren't inferred yet. The `tfplugin6` emitter
publishes them in both `GetProviderSchema` and `GetFunctions` (`emit_functions`),
and `service.rs` handles `CallFunction`: it decodes each argument with its
parameter's cty type, the erased `DynFunction` adapter assembles them into the
`Params` object (zipping by field name) and calls the typed handler, and the
result is encoded with the return type. Panics are contained as a
`FunctionError`. The whole path is exercised end to end by
`functions.tftest.hcl` (real `tofu` calling the function through an output).

Both shapes erase to `DynDataSource` (`data_source.rs`) and dispatch on
`ReadDataSource` (`service.rs`). Resources and data sources share a type-name
namespace per provider — `resource "aws_s3_bucket"` and `data "aws_s3_bucket"`
coexist (separate maps in the IR `ProviderSchema`); a plural data source
conventionally takes the plural name (`aws_s3_buckets`).

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
4. **TS iteration-sequence harness** (`harness/`, Vitest) — drives `example-fs`
   over a sequence of config folders that share one local-backend state file,
   asserting the JSON files the provider writes after each apply. This is the
   place to cover multi-step lifecycles (create → update → replace → delete)
   end-to-end; add a `configs/<name>/<n>/` folder with `*.tf` + `expected/`.
   Run with `cd harness && pnpm install && pnpm test` inside the dev shell. See
   `harness/README.md`.

The three engine-backed layers **require `tofu` or `terraform` on `PATH`** (the
dev shell provides it) and are the source of truth for protocol compatibility.
The shared `dev_overrides` workspace plumbing lives in `tests/common/mod.rs`
(Rust) and `harness/src/harness.ts` (TS).

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

**What to build next is tracked in [docs/ROADMAP.md](docs/ROADMAP.md)** — a
tiered backlog with per-feature context (current state, file/symbol anchors,
design sketch, gotchas, how to verify) written so a fresh session can pick up any
item cold. Start there before adding a feature.
