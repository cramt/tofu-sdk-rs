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

## Defining a resource

```rust
use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{async_trait, serve, Ctx, Provider, Resource, ResourceError};

/// The resource's schema, reflected from a plain Rust struct. The name comes
/// from the marker; bare `#[facet(terraform::resource)]` would infer it as
/// `snake_case` of the struct (`bucket`).
#[derive(Facet)]
#[facet(terraform::resource("aws_s3_bucket"))]
struct Bucket {
    // non-`Option` ⇒ inferred required
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

    async fn create(&self, ctx: &mut Ctx, mut planned: Bucket) -> Result<Bucket, ResourceError> {
        planned.arn = format!("arn:aws:s3:::{}", planned.name);
        // `ctx` carries success-path warnings, private state, and cancellation.
        ctx.warn("note", "this is a demo resource with no real backend");
        Ok(planned)
    }
    // read defaults to a passthrough, delete to a no-op.
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .resource(BucketResource) // name comes from the `Bucket` model's marker
        .build()
        .expect("provider definition is valid");
    serve(provider).await.expect("serve");
}
```

Point OpenTofu at the built binary with a `dev_overrides` CLI config and
`tofu apply` creates the resource — the schema is reflected from `Bucket`, and
the planned/prior state is decoded into `Bucket` and back automatically. No
schema boilerplate, no stringly-typed field plumbing.

## Data sources from the same model

Mark a struct as **both** a resource and a data source and a lookup key turns it
into read-only `data` sources — no second schema. An `exclusive` key (unique)
gives a singular lookup; a `shared` key (generic) gives a plural `…s` lookup with
a `results` list.

```rust
use terraform_runtime::{async_trait, Ctx, DataSource, DataSourceError};

#[derive(Facet)]
#[facet(terraform::resource("aws_s3_bucket"))]
#[facet(terraform::data_source("aws_s3_bucket"))] // also a data source
struct Bucket {
    #[facet(terraform::force_new)]
    #[facet(terraform::search_key(shared))]      // generic key → plural `aws_s3_buckets`
    name: String,
    #[facet(terraform::computed)]
    #[facet(terraform::search_key(exclusive))]   // unique key → singular `aws_s3_bucket`
    arn: String,
}

struct BucketByArn;

#[async_trait]
impl DataSource for BucketByArn {
    type Model = Bucket;
    async fn read(&self, _ctx: &mut Ctx, mut query: Bucket) -> Result<Bucket, DataSourceError> {
        // `arn` (the exclusive key) arrives set; fill the rest.
        query.name = query.arn.strip_prefix("arn:aws:s3:::").unwrap_or_default().into();
        Ok(query)
    }
}
// builder: .data_source(BucketByArn)              // data "aws_s3_bucket", keyed by arn
//          .data_source_list(BucketsByName)       // data "aws_s3_buckets", keyed by name
```

## Provider-defined functions

Pure functions callable from HCL as `provider::<name>::<fn>(…)`. Implement
`Function` over a `Params` struct (its fields are the positional parameters) and
an `Output` type — the signature is reflected from them.

```rust
use terraform_runtime::{async_trait, Function, FunctionError, VariadicFunction};

// provider::aws::arn_for("my-bucket")  ->  "arn:aws:s3:::my-bucket"
#[derive(Facet)]
struct ArnForArgs { name: String }

struct ArnFor;
#[async_trait]
impl Function for ArnFor {
    type Params = ArnForArgs;
    type Output = String;
    async fn call(&self, p: ArnForArgs) -> Result<String, FunctionError> {
        Ok(format!("arn:aws:s3:::{}", p.name))
    }
}

// Variadic: provider::aws::join("-", "a", "b", "c")  ->  "a-b-c"
// Leading params and the variadic element are separate types, so the const args
// and the var args can differ — and the type system enforces "exactly one
// variadic, always last".
#[derive(Facet)]
struct JoinArgs { separator: String }

struct Join;
#[async_trait]
impl VariadicFunction for Join {
    type Params = JoinArgs;   // leading params
    type VarArg = String;     // zero or more trailing args
    type Output = String;
    async fn call(&self, p: JoinArgs, parts: Vec<String>) -> Result<String, FunctionError> {
        Ok(parts.join(&p.separator))
    }
}
// builder: .function("arn_for", ArnFor).function_variadic("join", Join)
```

## Status

The original 5-phase MVP is complete and has grown well past it: a plain Rust
struct plus a `Resource` impl is a working provider, exercised end-to-end against
**real OpenTofu** (`apply` / `plan` / `destroy` / replacement). The full lifecycle
plus data sources, functions, nested blocks, a handler context, and lossless
64-bit numbers are in (see below). It is not yet battle-tested in production.

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
  by a real `tofu` test that flows a `provider` block region into a resource.
  `configure` may be fallible (`Result<Arc<Meta>, ConfigureError>`), and a
  provider-level `validate_config` hook rejects a bad provider block on
  `ValidateProviderConfig`
- **Data sources ✅** — read-only lookups dispatched on `ReadDataSource`,
  projectable from the *same* `Model` as the resource via
  `#[facet(terraform::search_key(exclusive|shared))]`: an `exclusive` key gives a
  singular `DataSource` (`read -> Model`, one object), a `shared` key gives a
  plural `DataSourceList` (`list -> Vec<Model>`, a `results` list) — both
  verified by real `tofu test` `data` reads
- **Import ✅** — `Resource::import(id)` dispatched on `ImportResourceState`
  (then refreshed via `read`) — verified by a real `tofu import`
- **State upgrades ✅** — a resource declares `SCHEMA_VERSION` and an
  `upgrade(from_version, prior)` migration; `UpgradeResourceState` runs it when
  stored state predates the current schema — verified by a real `tofu` refresh
  over pre-seeded v0 state
- **Config validation ✅** — `Resource`/`DataSource` `validate(config)` hooks
  return diagnostics (errors/warnings, optionally pointed at an attribute path)
  on `Validate{Resource,DataResource}Config` — verified by a real `tofu plan`
  that a `validate` hook rejects
- **Nested blocks ✅** — `#[facet(terraform::block)]` renders a field as an HCL
  nested block (`name { … }`); the Rust type fixes the nesting mode (struct /
  `Option<struct>` → single, `Vec` → list, set → set, `HashMap<String, _>` →
  map) and the element struct is reflected recursively (blocks may contain
  attributes and further blocks) — verified by a real `tofu apply` over single
  and list blocks. Required-ness of a *single* block is read from the type (a
  bare struct is required, `min_items = 1`; an `Option<struct>` is optional), and
  a block field projected into a data source stays a (computed) nested block
- **Provider-defined functions ✅** — pure functions dispatched on
  `GetFunctions`/`CallFunction`, callable from HCL as `provider::<name>::<fn>(…)`:
  implement `Function` (a `Params` struct → positional params, `Output` → return)
  or `VariadicFunction` (leading params + a `VarArg` element type) — both verified
  by a real `tofu` calling them over the plugin protocol
- **Handler context ✅** — every resource/data-source handler takes `&mut Ctx`:
  success-path warnings (`ctx.warn`, surfaced on a *successful* apply/read),
  per-resource private state (`ctx.private`/`ctx.set_private`), and cancellation
  (`ctx.is_cancelled`/`ctx.cancelled`) — verified by direct service tests
- **Lossless 64-bit numbers ✅** — `Value::Number` is `I64 | U64 | F64`, so the
  full signed+unsigned 64-bit range (64-bit IDs, large byte counts) round-trips
  through msgpack and cty JSON without the silent `f64` truncation above 2^53
- **Production hardening ✅** — a handler `panic!` is caught and returned as an
  error diagnostic instead of crashing the plugin; CRUD errors can point at an
  attribute path and carry warnings (`ResourceError::at`/`with_warning`);
  `tracing` is bridged to Terraform's JSON log stream on stderr under `TF_LOG`;
  and `StopProvider` trips a `CancellationToken` that in-flight handlers can
  observe via `current_cancellation()`

- **Plan modification, defaults & `TfValue<T>` ✅** — an optional attribute can
  carry a `#[facet(terraform::default("…"))]`; `Resource::modify_plan` adjusts the
  plan (force-replace by rule, mark computed-by-rule unknown); a `TfValue<T>`
  field preserves Terraform's known/unknown/null through decode (a plain `T` still
  zero-value-decodes); and computed attributes *inside nested blocks* are now
  planned unknown correctly

- **Ephemeral resources ✅** — values produced for the duration of a single
  operation and never written to state. Implement the `Ephemeral` trait
  (`open` → optional `renew` → `close`) over a `#[facet(terraform::ephemeral)]`
  model, registered with `ProviderBuilder::ephemeral` / `ephemeral_with` (or the
  `dyn_ephemeral` seam). `open` runs during *both* plan and apply, stashes a
  handle via `Ctx::set_private`, and may request renewal with `Ctx::renew_after`;
  `renew`/`close` receive only that private handle. Wrap an existing managed
  `Resource` in `EphemeralFromResource` to expose it ephemerally (Open = create,
  Close = delete) — for cheap, reversible resources only (no renewal; a created
  object leaks if the run is interrupted).

- **State stores ✅** — provider-defined Terraform *backends*. Implement the
  `StateStore` trait (`configure(config) -> Backend`, reflecting a `Config` block)
  to connect a `StateBackend` whose methods read/write the raw state bytes and
  manage locks per workspace (`read_state`/`write_state`/`lock`/`unlock`/`states`/
  `delete_state`), registered with `ProviderBuilder::state_store` /
  `state_store_with` (or the `dyn_state_store` seam). The runtime drives the full
  eight-RPC protocol — chunked `ReadStateBytes`/`WriteStateBytes`,
  lock/unlock/list/delete — over whole byte vectors. See the `inmem` example.

- **Actions ✅** — provider-defined imperative operations (an `action "<type>"
  "<label>" {}` block triggered by a resource's `lifecycle { action_trigger { … }
  }`). Implement the `Action` trait over a `Config` model: `validate` + a defaulted
  `plan` (dry run) + a required `invoke` (the side effect), which streams progress
  to the host via `ctx.progress(...)`. Registered with `ProviderBuilder::action` /
  `action_with` / `dyn_action`. The runtime drives `ValidateActionConfig`,
  `PlanAction`, and the server-streaming `InvokeAction`. See the `aws_publish`
  example. This completes the tfplugin6 protocol surface.

### Not yet implemented

There is no dedicated
semantic-equality / normalization hook for suppressing spurious diffs — though
modeling structured data as structured types (sets as sets, nested blocks as
blocks) plus `cty`'s native unordered-set semantics avoids most of the need.
Numbers fit `i64`/`u64`/`f64` (no arbitrary precision beyond 64-bit). Not all
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

| Package | Role |
|---------|------|
| `packages/tofu-sdk` (`@tofu-sdk/core`) | Write providers in **TypeScript** — a napi-rs Node addon over the dynamic seam (`native/`) plus a typed wrapper |

## Writing a provider in TypeScript

The Rust core is just one frontend on top of the `terraform-ir` + `Value` seam;
`@tofu-sdk/core` is another. You describe resources and data sources with a
**Zod** schema and async handlers — Zod gives you validation, inferred handler
types, *and* the `cty` schema (derived structurally) — and the Rust runtime
handles the whole plugin protocol. A TS provider is just a `node` script named
`terraform-provider-<name>`; no compiled binary.

```ts
import { z } from "zod";
import { Provider } from "@tofu-sdk/core";

const Bucket = z.object({
  name: z.string().meta({ forceNew: true }),   // disposition rides on the field
  arn: z.string().meta({ computed: true }),
  aliases: z.set(z.string()),                  // unordered → a cty set
});

await new Provider()
  .resource("aws_s3_bucket", {
    schema: Bucket,            // Zod → validation + inferred handler types + cty schema
    async create(planned) {    // planned.aliases is a Set<string>
      return { ...planned, arn: `arn:aws:s3:::${planned.name}` };
    },
    // read defaults to passthrough; update/delete/import are optional.
  })
  .serve();
```

**The Zod type defines everything** — structure, validation, handler types, *and*
the Terraform dispositions. Each one rides on its field via typed `.meta({ … })`
(`computed` / `forceNew` / `sensitive` / `writeOnly` / `deprecated` / `block` /
`set`), so a bad key is a compile error and "order must not matter" is just a
`z.set`. Here's a fuller provider: a `configure` step that builds an API client, a
repeatable nested `policy { … }` block, a computed+sensitive secret, a provider-
defined function, and a data source.

```ts
import { z } from "zod";
import { Provider, type Diagnostic } from "@tofu-sdk/core";

const Policy = z.object({
  effect: z.string(),
  permission_groups: z.set(z.string()),     // unordered → cty set
});

const ApiToken = z.object({
  name: z.string(),
  policy: z.array(Policy).meta({ block: true }),     // HCL `policy { … }` blocks
  id: z.string().meta({ computed: true }),
  value: z.string().meta({ computed: true, sensitive: true }),  // the secret
});

const TokenLookup = z.object({
  id: z.string(),                           // the input…
  name: z.string().meta({ computed: true }), // …the computed output
});

let client: ApiClient;

await new Provider()
  .config({
    schema: z.object({ api_token: z.string().optional() }),
    async configure(cfg) {
      client = new ApiClient(cfg.api_token ?? process.env.API_TOKEN!);
    },
  })
  .resource("example_api_token", {
    schema: ApiToken,
    async create(planned, ctx) {            // ctx: warnings / private state / cancel
      const created = await client.tokens.create(planned);
      ctx.warn("token minted", `id ${created.id}`);
      return { ...planned, id: created.id, value: created.secret };
    },
    validate(config): Diagnostic[] {
      return config.name.length === 0
        ? [{ severity: "error", summary: "name must not be empty", attribute: ["name"] }]
        : [];
    },
  })
  // A provider-defined function: `provider::example::token_arn("id")`.
  .function("token_arn", {
    params: z.object({ id: z.string() }),
    returns: z.string(),
    async call({ id }) { return `arn:example:token:${id}`; },
  })
  .dataSource("example_api_token", {
    schema: TokenLookup,
    async read(query) {
      const token = await client.tokens.get(query.id);
      return { ...query, name: token.name };
    },
  })
  .serve();
```

Covered today, at parity with the Rust frontend: resources (full lifecycle +
`upgrade`/`import`/`validate` + `modifyPlan`), singular and plural data sources
(`dataSource` / `dataSourceList`), provider config (with its own `validate`),
**ephemeral resources**, **provider-defined functions** (`function` /
`functionVariadic`), nested blocks, sets, every disposition via `.meta()`, the
handler `ctx` (warnings / private state / cancellation), and semantic-equality
normalization (a `z.transform` quotient field auto-suppresses diffs). Not yet
wired to the Node binding: list resources, state stores, resource identity, and
`moveState`. A complete, *real-backend* example — a Cloudflare provider talking to
the live `cloudflare` SDK — is in
[`packages/tofu-sdk/examples/cloudflare-provider.ts`](packages/tofu-sdk/examples/cloudflare-provider.ts).
Build and test with `pnpm build` / `pnpm test` (drives a real `tofu`).

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
