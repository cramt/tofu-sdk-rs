# Roadmap — landing the Rust core

Working backlog for the SDK, written so a fresh session (human or agent) can pick
up any item cold. Read [AGENTS.md](../AGENTS.md) first for crate layout, the
dynamic seam, and the gotchas; this file assumes that context.

References point at **files and symbols**, not line numbers (they rot). Grep for
the symbol.

## Definition of done ("Rust is landed")

**Tier 1 (all) + Tier 2 (all) + StopProvider/cancellation from Tier 3.** That
yields an SDK that can express real schemas, fail configuration cleanly, won't
crash on a panic, is debuggable, and cancels gracefully. Tiers 3–4 are additive
and don't reshape the public API.

Already shipped: full CRUD + planning, provider config + meta, data sources
(singular/plural from one model), import, state upgrades, config validation,
nested blocks, resource/data-source **name inference**, `sensitive`,
**fallible `configure`** (1.1), **provider-config `validate`** (1.3), **panic
safety** (2.1), **attribute-pathed CRUD errors + warnings** (2.2), **`tracing` →
Terraform JSON log bridge** (2.3), and **`StopProvider` + cancellation** (3.1).

**Status: the full "landed" cut is COMPLETE** — Tier 1 (all), Tier 2 (all), and
StopProvider/cancellation. 1.1/1.2/1.3/2.1/2.2/2.3/3.1 all merged and verified
against real OpenTofu (`cargo test --workspace` green, including the `tofu test`
e2e). Tier 1.2 shipped its three pieces: attribute defaults, `Resource::
modify_plan`, computed-attr-in-block consistency, and the `TfValue<T>` field
wrapper (known/unknown/null preserved through decode).

Remaining caveats (additive, not part of the landed cut): CRUD success-path
warnings (2.2 carries warnings only alongside an *error* today); `modify_plan`
operates on top-level attribute names and decodes the proposed model through the
zero-value rule (use `TfValue<T>` fields to read unknowns); a no-arg handler ctx
unifying cancellation + private state is a future refinement.

## How to verify (the four test layers)

Pick the cheapest layer that proves the feature; add to higher layers when the
behavior is protocol- or engine-observable. All engine layers need `tofu` on
PATH (the nix dev shell provides it). Run everything inside `nix develop`.

1. **Unit / direct trait calls** — `crates/terraform-runtime/tests/service.rs`
   constructs `ProviderService` and calls RPC methods directly (no socket).
   Reflection unit tests live in `crates/terraform-reflect/src/reader.rs`; codec
   in `crates/terraform-codec/src/typed.rs`.
2. **Native `tofu test` e2e** — `crates/example-aws/tests/tofu/*.tftest.hcl`
   (real apply/plan/destroy); runner `tofu_test.rs`.
3. **Schema contract** — `crates/example-aws/tests/tofu_schema.rs` parses
   `providers schema -json`.
4. **TS iteration harness** — `harness/` drives `example-fs` through multi-step
   shared-state sequences; assert JSON side effects. See `harness/README.md`.

Before committing: `cargo fmt --all`, `cargo clippy --workspace --all-targets`
(warning-free), `cargo test --workspace`, and `cd harness && pnpm test` when the
harness/provider changed.

## Cross-cutting gotchas (read before any Tier 1–2 work)

- **The dynamic seam is load-bearing.** Typed `Resource`/`DataSource`
  (`resource.rs`, `data_source.rs`) are erased to `DynResource`/`DynDataSource`
  (the `*Adapter` types). `service.rs` and `plan.rs` only ever see IR + `Value`.
  **Adding a method to `DynResource`/`DynDataSource` forces every implementor to
  add it too — including the Node binding** (`packages/tofu-sdk/native/src/lib.rs`,
  `impl DynResource for JsResource`). Default the trait method where possible so
  the erased trait and the binding don't both churn.
- **Computed-attr consistency.** Terraform rejects an applied value that differs
  from a *known* planned value ("inconsistent result after apply"). `plan.rs`
  only marks a computed attr unknown when it's null or the resource is replacing,
  and only walks **top-level** `block.attributes` (not inside nested blocks).
- **Numbers are `f64`** in the `Value` tree (`terraform-value/src/value.rs`).
- **No serde** — JSON is `facet-json` + `facet-value`; maps decode with
  `begin_key`/`begin_value` (not `begin_object_entry`).

---

## Tier 1 — real providers hit these immediately

### 1.1 Fallible `configure` — ✅ DONE
Shipped: `configure` accepts an infallible `Arc<M>` *or* `Result<Arc<M>, E>`
(`E: Into<Diag>`, e.g. `ConfigureError`) via the `IntoConfigured` shim; an `Err`
becomes a config diagnostic. `dyn_configure` unchanged.

- **Why:** a provider must be able to reject bad credentials / an unreachable
  endpoint with a diagnostic. Today it can't.
- **Current:** `ProviderBuilder::configure` (`builder.rs`) takes
  `F: Fn(C) -> Fut, Fut: Future<Output = Arc<M>>` and wraps it as
  `Ok(f(cfg).await)`. The internal `MetaFn<M>` **already** returns
  `BoxFuture<Result<Arc<M>, Diagnostics>>`, so the plumbing is ready.
- **Approach:** change the public `Fut::Output` to
  `Result<Arc<M>, E>` where `E: Into<Diag>` (or a `ConfigureError` with
  `summary`/`detail` like `ResourceError`). Map `Err` into the existing
  `Diagnostics` path. Keep an ergonomic success path (consider accepting either
  via a small `IntoConfigureResult` shim, or just require `Result`). `Provider::
  configure` already propagates `Diagnostics` to `ConfigureProvider`.
- **Gotcha:** `dyn_configure` (the seam used by the Node binding) already returns
  `Result<(), Diagnostics>` — leave it as is.
- **Verify:** `service.rs` test calling `configure_provider` with a config that
  makes the closure return `Err`, asserting the diagnostic. Optionally an
  `example-aws` `tofu test` that fails configure.
- **Done when:** an author can write `configure(|cfg| async { Err(..) })` and the
  diagnostic reaches Terraform.

### 1.2 Plan modification + attribute defaults + `TfValue<T>` — ✅ DONE
Shipped all three pieces:
- **`TfValue<T>`** (`terraform-value`, feature-gated `Facet` derive): a model
  field typed `TfValue<T>` round-trips `Known`/`Unknown`/`Null` through the codec
  (special-cased by type identifier in `terraform-codec`) and reflects to `T`'s
  cty type as a nullable attribute (`terraform-reflect`). Re-exported as
  `terraform_provider::TfValue`. Plain `T` still zero-value-decodes.
- **Defaults**: `AttributeSchema.default: Option<Value>` from
  `#[facet(terraform::default("…"))]` (parsed per cty type); applied in the
  planner to unset optional attributes, ahead of computed-unknown marking.
- **`Resource::modify_plan(prior, proposed) -> PlanModifications`**: force-replace
  by rule / mark computed-by-rule unknown, folded into the mechanical plan. New
  defaulted `DynResource::modify_plan` (Node binding unaffected).
- **Computed-in-block consistency**: `mark_computed_unknown` recurses into nested
  blocks, so the README/AGENTS limitation note no longer applies.

The big one; three intertwined pieces. Can land incrementally.

- **Why:** defaults are table-stakes; plan logic (computed-by-rule, diff
  suppression, replace-by-logic) is needed for real resources; `TfValue<T>`
  restores the known/unknown/null distinction that decode currently throws away
  (and is what lets plan logic be correct, including computed-inside-a-block).
- **Current:**
  - `plan.rs::plan` is mechanical: `requires_replace` (force_new value changed) +
    `mark_computed_unknown` (null or replacing, top-level only). No author hook.
  - No defaults anywhere: not in `AttributeSchema` (`terraform-ir/src/lib.rs`),
    not in `emit_attribute` (`terraform-tfplugin6/src/emit.rs`), not in the
    planner.
  - `from_value`/`fill` (`terraform-codec/src/typed.rs`) collapse
    `Unknown`/null → the type's zero value; the model can't represent unknown.
- **Approach (suggested order):**
  1. **`TfValue<T>`** — a wrapper (`Known(T)`/`Unknown`/`Null`) that
     reflects/decodes preserving the distinction. `terraform-value` already has a
     `TfValue` stub (see crate docs); wire it through `terraform-codec` so a field
     typed `TfValue<T>` round-trips unknown/null. Keep plain `T` working
     (zero-value decode) for ergonomics.
  2. **Defaults** — add `default: Option<Value>` (or a typed default) to
     `AttributeSchema`; reflect from a `#[facet(terraform::default(...))]` marker
     (extend the `terraform-attrs` grammar — note the struct-payload → direct-dep
     rule). Apply in the planner when the proposed value is null.
  3. **`Resource::modify_plan`** — a defaulted trait method
     `async fn modify_plan(&self, ctx) -> Result<PlannedState, Diag>` that runs
     inside `PlanResourceChange` after the mechanical pass. Decide a `ctx`
     carrying prior/config/proposed as `Value` or typed model + the ability to
     set `requires_replace` and unknown markers. **This adds a `DynResource`
     method → update `ResourceAdapter` and the Node binding** (default it to the
     mechanical plan so the binding can skip it).
- **Gotcha:** this is where "computed inside a nested block" gets fixed — make
  `mark_computed_unknown` recurse into `nested_blocks`, or fold it into
  `modify_plan`. Update the README/AGENTS limitation notes when done.
- **Verify:** plan.rs unit tests (defaults, modify_plan); a new `example-fs`
  resource with a default + a computed-in-block, exercised through the `harness/`
  (a config whose `expected/` proves the default applied and the computed-in-block
  stayed consistent — i.e. apply didn't error).
- **Done when:** an optional attribute can carry a default, a resource can adjust
  its own plan, and a computed attribute inside a block no longer trips
  "inconsistent result after apply".

### 1.3 Provider-config `validate` hook — ✅ DONE
Shipped: `ProviderBuilder::validate_config` (typed) + `dyn_validate_config`
(seam), erased behind `DynValidateConfig`, wired into `validate_provider_config`.

- **Why:** resources/data sources have `validate()`; the provider block has none.
- **Current:** `service.rs::validate_provider_config` returns `Default` (no-op).
- **Approach:** add a provider-level validate callback on `ProviderBuilder`
  (mirrors `dyn_configure`/`configure`), decode the config under
  `provider_config_ty()`, run it, return `Diagnostics`. Likely a `DynConfigure`-
  style erased hook so the Node binding can opt in.
- **Verify:** `service.rs` test calling `validate_provider_config`.
- **Done when:** a bad provider block is rejected with a diagnostic before
  configure.

---

## Tier 2 — production hardening (don't crash, be debuggable)

### 2.1 Panic safety — ✅ DONE
Shipped: `ProviderService::guard`/`guard_diags` wrap every handler dispatch with
`AssertUnwindSafe(..).catch_unwind()` (futures-util), turning a panic into an
error diagnostic. Requires `panic = "unwind"` (default).

- **Why:** a handler `panic!` currently unwinds out of the async task and can take
  down the plugin process; Terraform sees a dead transport, not a diagnostic.
- **Current:** no `catch_unwind` anywhere (`grep catch_unwind` is empty).
- **Approach:** wrap each erased handler dispatch in `service.rs` (or in the
  `*Adapter`s) with `FutureExt::catch_unwind` (futures crate) /
  `AssertUnwindSafe`, converting a panic into an error `Diag` with the panic
  message. Decide the boundary once and apply uniformly (create/read/update/
  delete/plan/import/upgrade/validate/data-source read).
- **Verify:** `service.rs` test with a handler that panics, asserting an error
  diagnostic (not a process abort). Note: requires `panic = "unwind"` (default).
- **Done when:** a panicking handler yields a clean diagnostic.

### 2.2 Richer CRUD diagnostics — ✅ DONE (partial)
Shipped (non-breaking): `ResourceError::at(path)` + `with_warning(diag)`;
`From<ResourceError> for Diagnostics` emits the error (with attribute path) plus
its warnings. `DynResource` and the Node binding untouched. **Deferred:**
warnings on a *successful* operation — needs the handler ctx from 1.2.

- **Why:** CRUD handlers can only return a flat `ResourceError` (summary/detail,
  no attribute path, no warnings). `validate` already returns `Vec<Diag>` with
  `Diag::at(path)`.
- **Current:** `Resource::{create,read,update,delete}` → `Result<_, ResourceError>`;
  the adapter maps `ResourceError → vec![Diag]` (`resource.rs`).
- **Approach:** let CRUD return `Diagnostics` (or keep `ResourceError` but enrich
  it with an optional attribute path + a warnings channel). Prefer a non-breaking
  path: add `ResourceError::at(...)` and a way to attach warnings, or introduce a
  result type. **Touches `DynResource` mapping and the Node binding** if the
  trait signatures change — prefer enriching `ResourceError` to avoid that.
- **Verify:** `service.rs` asserting an attribute-pathed diagnostic from a create.
- **Done when:** a handler can point an error at a specific attribute and emit
  warnings.

### 2.3 `tracing` → Terraform log bridge — ✅ DONE
Shipped: a hand-rolled `tracing::Subscriber` (`log.rs`, no `tracing-subscriber`
dep) emits hclog JSON (`@level`/`@message`/`@module`/`@timestamp`) to **stderr**,
gated on `TF_LOG_PROVIDER`/`TF_LOG`; installed in `serve.rs`; RPC entry points
instrumented with `tracing::debug!`.

- **Why:** zero logging in the runtime today; `TF_LOG` shows nothing from the
  provider, so real debugging is blind.
- **Current:** no `tracing`/`log` dep in `terraform-runtime`.
- **Approach:** add `tracing` + a subscriber that writes Terraform's JSON log
  format (`@level`, `@message`, `@module`, timestamp) to **stderr** (go-plugin
  captures stderr; do NOT write to stdout — that's the handshake/gRPC channel).
  Initialize in `serve.rs`. Respect `TF_LOG`/`TF_LOG_PROVIDER` levels. Instrument
  RPC entry/exit and handler dispatch.
- **Gotcha:** stdout is sacred (handshake line + gRPC). Logs go to stderr only.
- **Verify:** manual `TF_LOG=trace tofu apply` against `example-aws` shows
  structured provider logs; a unit test that the subscriber emits valid JSON.
- **Done when:** provider logs appear under `TF_LOG` in Terraform's stream.

---

## Tier 3 — protocol completeness (graceful operation)

### 3.1 StopProvider + cancellation *(part of the "landed" cut)* — ✅ DONE
Shipped: `ProviderService` holds a `CancellationToken`; `stop_provider` trips it
and acks. Each dispatch is scoped under a `CANCEL` task-local; handlers read it
via `terraform_runtime::current_cancellation()` (re-exports `CancellationToken`).

- **Current:** `stop_provider` is in the `unimplemented_unary!` list in
  `service.rs` (returns `Unimplemented`). Shutdown is SIGTERM/Ctrl-C only
  (`serve.rs`); do not watch stdin.
- **Approach:** implement `stop_provider` to trip a `CancellationToken`
  (tokio-util) shared into handler dispatch; pass it via handler `ctx` (ties in
  with 1.2's plan ctx — consider a shared request context now). Acknowledge with
  an empty `Response`.
- **Verify:** `service.rs` test that `stop_provider` returns OK and flips the
  token.
- **Done when:** `StopProvider` no longer errors and in-flight handlers can
  observe cancellation.

### 3.2 `MoveResourceState`
- `moved {}` blocks and cross-resource-type state moves. In `unimplemented_unary!`.
  Add a `Resource::move_state(from_type, from_state) -> Model` hook + dispatch.

### 3.3 Number precision
- `Value::Number(f64)` is lossy for large ints / high-precision decimals. Consider
  a decimal/bignum backing (cty uses `big.Float`). Wide blast radius
  (`terraform-value`, codec, every `as f64`) — scope carefully.

### 3.4 Misc completeness
- **Private state to handlers:** `service.rs` round-trips Terraform's per-resource
  `private` bytes (`planned_private`/`private`) but never exposes them. Surface
  read/write via handler ctx (needed for timeouts/SDKv2-style bookkeeping).
- **`timeouts {}`:** the common per-operation timeout block convention. Now
  expressible as a nested block; needs runtime plumbing to read + enforce.
- **Schema flags:** `emit_attribute` hardcodes `deprecated: false` /
  `write_only: false`; `emit_nested_block` hardcodes `min_items: 0`/`max_items: 0`.
  Add `deprecated`/`write_only` markers and required-block (`min_items`) support.

---

## Tier 4 — bigger / newer surfaces (defer unless wanted)

All are stubbed in `service.rs` (`unimplemented_unary!` or streaming stubs).

- **Provider-defined functions** (`GetFunctions`/`CallFunction`) — self-contained
  and demoable; a good showcase. Needs a `Function` trait + arg/return codec.
- **Ephemeral resources** (`Open/Renew/CloseEphemeralResource`).
- **List resources** (`ListResource`, streaming).
- **Resource identity** (`GetResourceIdentitySchemas`/`UpgradeResourceIdentity`).
- **State store** (`ReadStateBytes`/`WriteStateBytes`/`Lock`/`Unlock`/…, streaming).
- **Actions** (`PlanAction`/`InvokeAction`/`ValidateActionConfig`).
