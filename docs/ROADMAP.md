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

Remaining caveats (additive, not part of the landed cut): `modify_plan` decodes
the proposed model through the zero-value rule (use `TfValue<T>` fields to read
unknowns); its `PlanModifications` now target attribute `Path`s, so nested
attributes are reachable (see **3.5**).

The **handler `Ctx`** (`terraform-runtime/src/ctx.rs`) now unifies what used to be
a list of separate gaps: every handler takes `ctx: &mut Ctx`, giving it
success-path warnings (`ctx.warn`, surfaced on *successful* applies/reads — no
longer error-only), per-resource private state (`ctx.private` /
`ctx.set_private`, persisted across operations), and cancellation
(`ctx.is_cancelled` / `ctx.cancelled`). It is injected ambiently via a task-local
so the erased `DynResource`/`DynDataSource` seam is unchanged.

## Beyond "landed": real-provider readiness gaps

"Landed" makes a *small/medium* provider over a REST/SaaS API shippable today
(string/bool/list/map/object/nested-block fields; numbers that fit in 53 bits).
The items below are what a *large or precision-sensitive* provider (think
`aws`/`google`) hits in practice. None reshape the public API; all are additive.
Rough priority order, each pointing at its tracked item:

1. ~~**`f64` numbers — silent precision loss.**~~ ✅ **DONE** — `Value::Number`
   is now `enum Number { I64, U64, F64 }`, so the full 64-bit integer range
   round-trips losslessly. Truly arbitrary precision (beyond 64-bit) remains
   out of reach, matching the JSON layer's own ceiling. → **3.3**.
2. ~~**Success-path warnings.**~~ ✅ **DONE** — every handler takes `ctx: &mut
   Ctx`; `ctx.warn(...)` surfaces a warning alongside a *successful* apply/read,
   and `ctx.private`/`ctx.set_private` carry per-resource private state. The same
   `Ctx` also exposes cancellation. → **2.2** + the handler-ctx keystone.
3. ~~**Plan modification depth.**~~ ✅ **DONE** — `PlanModifications` now targets
   attribute `Path`s (`Attribute`/`Index`/`Key` steps), so `modify_plan` can
   force-replace or mark unknown a *nested* attribute (inside a block or
   collection), not just a top-level one. Known/unknown access stays via
   `TfValue<T>` fields. → **3.5**.
4. ~~**Semantic equality / normalization.**~~ ✅ **DONE** — *quotient
   types*: a value modeled as a newtype whose constructor canonicalizes makes
   semantic equality free (`canonical(a) == canonical(b)`), and the planner keeps
   the prior value when a change is within the equivalence class. Now **zero-wiring**:
   the codec proxy bridge makes a quotient type a usable model field, and
   `Resource::semantic_equality` defaults to auto-harvesting the `Canon` from
   `M::SHAPE`. → **3.6**.
5. ~~**Write-only attributes.**~~ ✅ **DONE** — the schema flag
   (`#[facet(terraform::write_only)]` → IR/emit/TS) *and* the runtime semantics:
   the value is nulled out of the planned and every returned state but merged
   from the apply-time config into the handler's input. → **3.4**.
6. ~~**Nested-block fidelity.**~~ ✅ **DONE** — a `NestedBlock` carries
   `min_items`/`max_items`: a bare-struct single block is required
   (`min_items = 1`), `Option<struct>` optional; collections stay unbounded. The
   singular data-source projection keeps a `block` field as a read-only nested
   block instead of an object attribute (the plural `results` list keeps it as an
   object attribute — unavoidable for a `list(object(...))`). → **3.7**.
7. **Modern protocol surfaces** — ~~provider-defined functions~~ ✅ **DONE**
   (`GetFunctions`/`CallFunction`, typed `Function` + `VariadicFunction` traits);
   ~~ephemeral resources~~ ✅ **DONE** (`Open`/`Renew`/`Close`, typed `Ephemeral`
   trait + `EphemeralFromResource` adapter, dynamic seam via `dyn_ephemeral`);
   ~~cross-type state move~~ ✅ **DONE** (`MoveResourceState`, typed
   `Resource::move_state`). → **3.2** + **Tier 4**.

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

### 3.2 `MoveResourceState` — ✅ DONE
- **Done:** `moved {}` cross-resource-type state moves. New defaulted
  `Resource::move_state(ctx, source_type_name, source_state: Value) -> Model`
  hook (typed); `source_state` is the source resource's raw stored state decoded
  **untyped** (via `decode_json_value`, as `upgrade` does — the source schema may
  be foreign), so a handler typically matches on `source_type_name` and maps the
  dynamic value onto its target model. The default errors ("unsupported"). Erased
  as a defaulted `DynResource::move_state` (Node binding unaffected);
  `service.rs::move_resource_state` decodes the source `RawState` JSON, dispatches
  through the `run` helper (so warnings/private/cancellation work), and encodes
  the target model into `target_state` (echoing `source_private` unless the
  handler rewrites it).
- **Verified:** direct service tests (`move_resource_state_migrates_across_types`
  maps a foreign `legacy_widget`'s `label` onto `widget.name`;
  `move_resource_state_unsupported_yields_diagnostic` checks the default errors).
  Engine-level (`tofu test` with a real `moved {}`) is deferred — it needs a
  two-type multi-step state scenario better suited to the TS harness.

### 3.3 Number precision — ✅ DONE
- **Done:** `Value::Number` now holds `enum Number { I64, U64, F64 }`
  (`terraform-value/src/value.rs`), mirroring the msgpack int/uint/float forms
  and the JSON value layer's own number cases. The full signed+unsigned 64-bit
  integer range round-trips losslessly through both wire formats. The codec
  keeps integers in their exact case (`number_from_mp`/`number_to_mp` for
  msgpack, `json_number`/`number_to_json` for cty JSON); the typed-model
  boundary converts directly per width (`to_i64_lossy`/`to_u64_lossy`/
  `to_f64_lossy`, plus `wide_int_to_value` narrowing `i128`/`u128`). `Number`
  equality is by mathematical value across cases, so a 64-bit ID is never
  conflated with an `f64`-rounded neighbour. Authors keep using plain
  `i64`/`u64`/`f64` fields; the only lossy step is at the typed boundary when the
  declared field type genuinely can't hold the value.
- **Verified:** `large_integers_round_trip_without_precision_loss`
  (`terraform-codec/src/lib.rs`) round-trips `2^53 + 1` and `u64::MAX` through
  msgpack and cty JSON; `large_integers_compare_exactly_not_via_f64`
  (`terraform-value/src/value.rs`) guards the exact-comparison semantics.
- **Remaining limit (acceptable):** truly arbitrary precision (beyond
  `i64`/`u64`/`f64`) is still not representable. That matches the ceiling of the
  JSON value layer (`facet-value`'s `VNumber` is itself `I64`/`U64`/`F64`) and is
  fine for real configs; lifting it would require changing `facet-value`.

### 3.4 Misc completeness
- ~~**Private state to handlers.**~~ ✅ **DONE** — exposed via the handler `Ctx`:
  `ctx.private()` reads the incoming `private`/`planned_private` bytes and
  `ctx.set_private(...)` persists new ones, threaded through the apply/read/plan
  responses in `service.rs` (regression: `read_observes_incoming_private_state`,
  `create_success_carries_warning_and_persists_private_state`).
- ~~**`timeouts {}`.**~~ ✅ **DONE** — the per-operation deadline block
  (`terraform-runtime/src/timeouts.rs`). Authors embed the ready-made
  `terraform_runtime::Timeouts` as an optional nested block
  (`#[facet(terraform::block)] timeouts: Option<Timeouts>`); the runtime reads the
  relevant Go-style duration (`"30s"`/`"1h30m"`/`"500ms"`) off the dynamic `Value`
  at apply/read time and wraps the handler in `tokio::time::timeout`
  (`timeouts::bounded`, inside the panic-guard/ctx scope), turning an overrun into a
  clean error diagnostic. create/update read the deadline from the planned state,
  delete from the prior, read from the current. Absent/blank/zero → unbounded.
  Verified by parser/extraction unit tests and an end-to-end service test
  (`timeouts_block_bounds_a_slow_create`).
- ~~**Write-only attributes.**~~ ✅ **DONE** — both pieces. *Schema flag:*
  `AttributeSchema.write_only` (`terraform-ir`), reflected from
  `#[facet(terraform::write_only)]` (`terraform-reflect`; rejects
  `write_only`+`computed` via `ReflectError::WriteOnlyComputed`, and the
  data-source projections clear it), emitted by `emit_attribute`
  (`terraform-tfplugin6`), and exposed in the TS frontend as a `writeOnly`
  disposition + the addon's `writeOnly` JSON flag. *Runtime semantics*
  (`terraform-runtime/src/write_only.rs`): the planned state and every returned
  state have write-only attributes nulled (`strip`, recursing into nested
  blocks), while `apply` merges the real value from the apply-time **config**
  into the planned value (`merge_from_config`) so a `create`/`update` handler
  still receives it. Wired into `plan.rs` and `service.rs` (apply + read), gated
  on `block_has` so no-write-only resources are untouched. Verified by unit tests
  (`write_only.rs`, `reader.rs`), an end-to-end service test
  (`write_only_value_reaches_handler_but_not_state`), the schema contract test
  (`aws_locker.secret` → `write_only: true`), and a real `tofu test`
  (`write_only.tftest.hcl`: handler sees the secret, state nulls it).
- ~~**`deprecated` flag.**~~ ✅ **DONE** — `AttributeSchema.deprecated:
  Option<String>` (the message; `None` = not deprecated), reflected from
  `#[facet(terraform::deprecated)]` / `deprecated("msg")` (`field_deprecated` in
  `terraform-reflect`), emitted as `deprecated` + `deprecation_message`
  (`emit_attribute`), and exposed in the TS frontend as a `deprecated`
  disposition + the addon's `deprecated` JSON flag. Verified by a reflect unit
  test (`deprecated_marker_carries_optional_message`) and the schema contract
  test (`aws_locker.legacy_name` → `deprecated: true`). (Required-block
  `min_items` is done — see 3.7 / nested-block fidelity.)

### 3.5 Plan modification depth — ✅ DONE
- **Done:** `PlanModifications` (`resource.rs`) now targets attribute **`Path`s**
  instead of bare names. A `Path` is a sequence of `PathStep`s
  (`Attribute(name)` / `Index(i)` / `Key(k)`) built fluently
  (`Path::root().attribute("settings").index(0).attribute("id")`); `From<&str>`/
  `From<String>` keep the common top-level case ergonomic, so existing
  `require_replace("tier")` / `unknown("foo")` call sites are unchanged.
  `apply_modifications` (`plan.rs`) walks `plan.planned` along each unknown path
  via `set_at_path` (a step that doesn't resolve is a silent no-op) and converts
  each `require_replace` path to a protocol `AttributePath` via `to_attribute_path`
  (deduped against the mechanical `force_new` paths). `Path`/`PathStep` are
  re-exported from `terraform_runtime`. The `DynResource::modify_plan` signature
  is unchanged (it already passed `Value`s), so the **Node binding is
  unaffected**.
- **Typed unknown access:** preserved as before via `TfValue<T>` fields — a model
  field typed `TfValue<T>` decodes known/unknown/null faithfully inside
  `modify_plan`, so a rule can branch on whether a value is yet known. (Plain
  fields still zero-value-decode.)
- **Verified:** `plan.rs` unit tests (`modification_marks_nested_block_attribute_unknown`,
  `modification_require_replace_targets_nested_path`,
  `modification_with_unresolvable_path_is_a_noop`,
  `require_replace_dedupes_against_mechanical_paths`, plus the top-level
  back-compat case) and an end-to-end service test
  (`modify_plan_marks_nested_block_attribute_unknown` in
  `terraform-runtime/tests/service.rs`) that drives `PlanResourceChange` and
  asserts `settings[0].id` comes back unknown through the encode round-trip.
- **Not in scope (separate items):** diff-suppression / "keep prior value" is the
  normalization concern of 3.6 (deliberately not done — handled by correct data
  modeling); passing the raw `config` (distinct from `proposed`) into
  `modify_plan` was not needed and is left out.

### 3.6 Semantic equality / normalization — ✅ DONE (zero-wiring via auto-harvest)
- **Why:** Terraform core (not the provider) diffs by structural value equality
  over `cty`. Providers routinely need to treat differently-encoded-but-equal
  values as unchanged — equivalent set ordering, case-insensitive IDs, normalized
  ARNs/URLs. Without a hook, every such attribute shows a spurious perpetual diff
  (and a `force_new` such attribute spuriously *replaces*).
- **Design (shipped):** *quotient types*, not a free-floating hook. The provider's
  only lever is `PlanResourceChange` — return the prior value when the new value
  is semantically equal (the blessed move; what SDKv2 `DiffSuppressFunc` and the
  Plugin Framework `StringSemanticEquals` do). A value modeled as a **quotient
  type** (a newtype whose constructor maps an equivalence class to one canonical
  representative) makes equality free: `canonical(a) == canonical(b)`, derived
  from the type — the author writes no equality function.
  - `terraform-runtime/src/normalize.rs`: `keep_prior` pre-pass (run *before*
    `plan::plan`) rewrites a semantically-equal proposed attribute back to the
    **prior bytes** ("store-raw, normalize-on-compare" — never plans a third
    value, so no "inconsistent result after apply"). `string_quotient::<T>()`
    builds an `Arc` canonicalizer from a type's `TryFrom<String>` / `TryFrom<&T>`
    conversions — the same ones facet's `#[facet(opaque, proxy = String)]` uses.
    `Canon` (`#[must_use]`, `Clone`) maps attr name → canonicalizer.
  - Wired via a defaulted `Resource::semantic_equality(&self) -> Canon`, forwarded
    through the `DynResource` seam (defaulted → Node binding unaffected) and
    `ResourceAdapter`, called in `service.rs` before the mechanical plan.
- **Verified:** `normalize.rs` unit tests + an end-to-end service test
  (`semantic_equality_suppresses_spurious_replacement_in_plan`) driving
  `PlanResourceChange`: a case-only change to a `force_new` attr plans as
  no-change (planned keeps prior bytes), a real change still replaces.
- **Codec proxy-decode support — ✅ DONE.** `terraform-codec` now drives facet's
  container-level proxy vtable: `peek_to_value` calls `custom_serialization_from_shape`
  (→ `convert_out`) and `fill` calls `begin_custom_deserialization_from_shape`
  (→ `convert_in`), both in `typed.rs`; `terraform-reflect::map_type` maps an
  `opaque+proxy` field to its proxy's cty type. So an `opaque+proxy` quotient type
  round-trips through the codec and **can be a real model field** (decode runs the
  canonicalizing `TryFrom`, encode renders it back). Verified by
  `terraform-codec` proxy round-trip tests and a `terraform-reflect` type test.
- **Reflection auto-harvest — ✅ DONE.** `Canon::harvest::<M>()` (`normalize.rs`)
  walks `M::SHAPE`, detects each top-level quotient field (a container-proxy type,
  optionally `Option`-wrapped, via `quotient_inner`) and registers a canonicalizer
  built from the type-erased `terraform_codec::canonicalize_through_shape` (a
  shape-driven codec round-trip via `Partial::alloc_shape`). It is the **default**
  behind `Resource::semantic_equality`, so a quotient field needs *zero* per-resource
  wiring; an override can still add canonicalizers reflection can't see
  (`Canon::harvest::<Self::Model>().with("id", string_quotient::<MyId>())`). A model
  with no quotient fields harvests an empty `Canon` (pre-pass skipped, zero overhead).
  Verified by `normalize.rs` harvest tests (incl. end-to-end through `keep_prior`).
- **`Canon` caching — ✅ DONE.** `ResourceAdapter::erased` computes the `Canon`
  once at construction (`semantic_equality` is a static description of the model);
  `DynResource::semantic_equality` hands out a cheap `Arc`-backed clone per plan, so
  there is no per-plan `SHAPE` walk.
- **Single nested-block recursion — ✅ DONE.** `harvest` + `keep_prior` recurse
  into a single nested struct/block (`Struct`/`Option<Struct>`) so a quotient inside
  a config block is suppressed too (`single_struct_inner` gates it).
- **Repeated (list/set) blocks — NOT a "do element matching" item.** Element
  matching / multiset comparison is the wrong altitude — it re-introduces the
  bespoke equality function this whole design deletes. Equality must reduce to
  `PartialEq` on a well-chosen type (see the **"Equality is `PartialEq`"** design
  law in AGENTS.md): order-must-not-matter → the author models it `HashSet<T>`,
  not `Vec<T>`, and the element is a quotient type. If we ever suppress repeated
  blocks, do it by canonicalizing the whole value through the model `SHAPE`
  (parse → typed → re-encode) + one `PartialEq`, never hand-rolled matching.
- **Deferred (promotion follow-ups, see `normalize.rs` docs):**
  1. `TryFrom<&str>`/`Cow` to avoid the per-call string clone.
  2. Meta-backed resources skip suppression until ConfigureProvider (configure
     precedes plan in the normal workflow, so this only affects a pre-configure
     partial plan).
- **Out of scope (needs `modify_plan`):** *server-authoritative* normalization,
  where only the remote knows the canonical form — no client-side `parse` can
  reproduce it.

### 3.7 Data-source block projection — ✅ DONE (singular)
- **Done:** `reflect_data_source` now honors the `block` marker, projecting a
  block field as a read-only `NestedBlock` (every inner attribute computed,
  recursively, `min_items` forced to 0) via `as_computed_block` — matching the
  resource path instead of collapsing to an object attribute. Regression:
  `singular_projection_keeps_block_as_computed_nested_block` in
  `terraform-reflect/src/reader.rs`.
- **Plural caveat (by design):** the *plural* projection still renders a block
  field as an object attribute inside the computed `results` `list(object(...))`.
  A repeated HCL block can't be an element of a computed list, so the structure
  is carried as typed data — this is correct, not a gap.

---

## Tier 4 — bigger / newer surfaces (defer unless wanted)

All are stubbed in `service.rs` (`unimplemented_unary!` or streaming stubs).

- ~~**Provider-defined functions** (`GetFunctions`/`CallFunction`).~~ ✅ **DONE** —
  typed `Function` trait (`Params` struct → positional params, `Output` → return)
  over an erased `DynFunction` seam; `reflect_function` builds the
  `FunctionSignature` IR, `emit_functions` publishes it, `service.rs` dispatches
  `CallFunction`. Registered with `ProviderBuilder::function` / `dyn_function`.
  **Variadic** functions are supported too via a separate `VariadicFunction`
  trait (leading `Params` struct + `VarArg` element type), registered with
  `function_variadic` — the type system enforces one-uniform-trailing-variadic
  (no marker). Verified by direct service tests (incl. heterogeneous leading +
  variadic types and zero-arity), the schema contract test, and
  `functions.tftest.hcl` (real `tofu` calling both `arn_for` and the variadic
  `join`). Examples: `example-aws`'s `arn_for` and `join`.
- ~~**Ephemeral resources** (`Open/Renew/CloseEphemeralResource`).~~ ✅ **DONE** —
  typed `Ephemeral` trait (`open` → optional `renew` → `close`, plus `validate`)
  over an erased `DynEphemeral` seam; `reflect_ephemeral` builds the
  `EphemeralSchema` IR (plain fields = config inputs, `computed` = result),
  `emit.rs` publishes it in `ephemeral_resource_schemas` + `GetMetadata`, and
  `service.rs` dispatches all four RPCs. `open` runs during plan *and* apply and
  threads a handle through `Ctx::set_private`; `Ctx::set_renew_at`/`renew_after`
  drive the `renew_at` deadline. Registered with `ProviderBuilder::ephemeral` /
  `ephemeral_with` / `dyn_ephemeral`. `EphemeralFromResource<R>` adapts a managed
  `Resource` (Open = create, Close = delete) for the cheap-reversible case (no
  renew; leaks on interrupt). Verified by direct service tests (open/renew/close,
  private round-trip, the wrapper) and the schema contract test. Example:
  `example-aws`'s `aws_session_token`.
- ~~**List resources** (`ListResource`, streaming).~~ ✅ **DONE** — typed
  `ListResource` trait (`list.rs`): `type Model` (the managed resource's model,
  reused so identity + object type line up by construction) + `type Config` (the
  `list {}` query block) → `list(ctx, config) -> Vec<ListItem<Model>>`. Erased
  behind `DynListResource`/`ListResourceAdapter`; `reflect_list_resource` builds
  the `ListResourceSchema` IR (config block published as the list schema, identity
  + object type harvested from `Model`; a model with no `#[facet(terraform::
  identity)]` is a `ReflectError::ListResourceWithoutIdentity`). `emit.rs`
  publishes `list_resource_schemas` (GetProviderSchema) + `list_resources`
  (GetMetadata); `service.rs::list_resource` decodes the config, dispatches, and
  streams one `Event` per result (projecting identity via `known_identity_data`,
  encoding the full object into `resource_object` only when the host sets
  `include_resource_object`, honoring `limit`). Registered with
  `ProviderBuilder::list_resource` / `list_resource_with` / `dyn_list_resource`.
  Example: `example-aws`'s `aws_locker` list resource. **Verified** at the direct
  service-call layer (`list_resource_*` tests in `terraform-runtime/tests/
  service.rs`: filtered stream + identity, `resource_object` on request, `limit`,
  unknown-type diagnostic) and a protocol schema-contract test
  (`list_resource_is_published_in_schema_and_metadata`). **Engine layers (2/3)
  deferred:** OpenTofu 1.12.1's `providers schema -json` drops `list_resource_
  schemas` entirely (the surface is too new to drive `tofu`), so the protocol
  assertion against our own `GetProviderSchema` stands in — like `MoveResourceState`.
- ~~**Resource identity**~~ ✅ **DONE** (`GetResourceIdentitySchemas` /
  `UpgradeResourceIdentity`). Type-driven: a model marks identity fields with
  `#[facet(terraform::identity)]` and `reflect_resource` projects them into an
  `IdentitySchema` (IR) → `emit_identity_schemas` (tfplugin6). The runtime returns
  the identity (`known_identity_data`, omitted while any key is unknown — e.g.
  plan-on-create) on `plan` (`planned_identity`), `apply`/`read` (`new_identity`),
  and `import` (`ImportedResource.identity`); `UpgradeResourceIdentity` is a
  decode→re-encode passthrough (identity stays version 0, no author migration
  hook). Opt-in; resources without identity-marked fields are unaffected.
  Verified by reflect unit tests, direct service tests (schema, apply identity,
  plan-omits-while-unknown), and the real `tofu test` suite (`aws_locker` now
  declares `name` as its identity and applies/destroys cleanly, so OpenTofu
  validates the planned/new identity consistency).
- ~~**State store**~~ ✅ **DONE** — provider-defined Terraform backends. A
  **two-trait split** mirroring provider config → meta: the typed `StateStore`
  trait (`type Config` → published config block; `configure(config) -> Backend`)
  builds a connected `StateBackend` on `ConfigureStateStore`, and `StateBackend`
  holds the byte/lock operations keyed by `state_id`
  (`read_state`/`write_state`/`lock`/`unlock`/`states`/`delete_state`). Erased
  behind `DynStateStore`/`DynStateBackend`; new IR `StateStoreSchema`
  (`ProviderSchema.state_stores`), `reflect_state_store` (config block only, like
  `reflect_ephemeral`; name supplied at registration like a function),
  `emit.rs` publishes `state_store_schemas` (GetProviderSchema) + `state_stores`
  (GetMetadata). `service.rs` implements all eight RPCs:
  `ValidateStateStoreConfig`/`ConfigureStateStore`, the streaming
  `ReadStateBytes` (whole state read then chunked at the negotiated `chunk_size`)
  / `WriteStateBytes` (chunks reassembled — body in the testable
  `write_state_stream` since `tonic::Streaming` has no public constructor),
  `LockState`/`UnlockState`, `GetStates`/`DeleteState`. The connected backend is
  stored at runtime per type name in `ProviderService.state_backends` (an
  `RwLock<HashMap>`), populated by `ConfigureStateStore` — separate from
  `ConfigureProvider`'s meta. Registered with `ProviderBuilder::state_store` /
  `state_store_with` / `dyn_state_store`. Example: `example-aws`'s `inmem` store.
  **Verified** by direct service tests (`state_store_*` in
  `terraform-runtime/tests/service.rs`: full lifecycle, chunked read, validate
  rejection, read-before-configure, unknown-type, schema/metadata publication), a
  reflect unit test, and the protocol schema-contract assertion. **Engine layer
  deferred:** OpenTofu 1.12.1's `providers schema -json` drops
  `state_store_schemas` (too new to drive `tofu`), so the protocol assertion
  against our own `GetProviderSchema` stands in — like list resources.
- **Actions** (`PlanAction`/`InvokeAction`/`ValidateActionConfig`). The last
  unimplemented protocol surface — Terraform's imperative-action primitive. Its
  own IR + RPCs, like state stores; defer unless wanted.

---

## TypeScript frontend (`@tofu-sdk/core`) — Rust parity

The Node binding (`packages/tofu-sdk/native`) is a thin napi-rs bridge over the
dynamic seam; the TS wrapper (`packages/tofu-sdk/ts`) is the author API. The
guiding rule mirrors the Rust one: **the hard part stays in Rust**, and **the Zod
type defines everything** (structure, validation, handler types, *and* dispositions
via `.meta({…})`). Schema derivation reads the Zod type directly (`ts/schema.ts`,
a pure native-free module unit-tested via `dist/schema.js`), not `z.toJSONSchema`.

**At parity (verified end-to-end against real `tofu` in `test/e2e.test.mjs`):**
resources (CRUD + `import` + `version`/`upgrade` + `validate` + `modifyPlan`),
provider config + `configure` + `validate`, singular/plural data sources,
ephemeral resources, **provider-defined functions** (`function` /
`functionVariadic`), all attribute dispositions via field `.meta()`, nested blocks
(single/list/set), **unordered sets** (`z.set` scalar + the `set` disposition,
with JS `Set`⇄array marshaling), the handler **`ctx`** (success-path warnings,
private state, cancellation — threaded via a `{ctx, value}` envelope +
`run_until_cancelled`), **semantic-equality normalization** (a `z.transform`
quotient field auto-suppresses diffs through `modifyPlan`'s `keepPrior`, the TS
mirror of `Canon::harvest`), **`moveState`** (cross-type `moved {}`): an
optional `moveState(sourceTypeName, sourceState, ctx)` hook over the defaulted
`DynResource::move_state` seam (verified by `test/e2e.test.mjs`'s "migrates state
across resource types via a moved block" — also the **first engine-level (`tofu`)
test of `MoveResourceState`** in the repo), and **resource identity** (an
`identity` field disposition → `IdentitySchema` over the new
`dyn_resource_with_identity` seam; the runtime projects identity off the returned
`Value`, so no handler method is added). The identity test asserts `tofu show
-json` records the projected identity — the engine can't surface identity in
`providers schema -json` (1.12.1 drops it, like list resources / state stores), so
the in-state projection stands in.

**Still unimplemented in the binding** (the Rust core supports each):
1. **State stores** + **list resources** — new primitives over `dyn_state_store` /
   `dyn_list_resource` (addon + TS; the seams exist). Not engine-testable yet
   (OpenTofu 1.12.1 drops both schemas from `providers schema -json`).

**Normalization caveat:** diff suppression via `keepPrior` is cleanest for
computed / diff-stable values; for a plain required input, Terraform core's
plan-consistency check still compares the planned value to config (a Terraform-core
boundary, the same on the Rust side, which ships normalization with service-level
tests).
