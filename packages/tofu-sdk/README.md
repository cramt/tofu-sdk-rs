# @tofu-sdk/core

Write Terraform/OpenTofu providers in TypeScript, backed by a Rust `tfplugin6`
core. You describe each resource and data source with a schema and async
handlers; the native addon runs the real plugin server (go-plugin handshake,
auto-mTLS, msgpack codec, planning) and drives your handlers with decoded
values. No protocol, gRPC, or `cty` encoding to deal with.

```ts
import { z } from "zod";
import { Provider } from "@tofu-sdk/core";

const Bucket = z.object({
  name: z.string().meta({ forceNew: true }),
  arn: z.string().meta({ computed: true }),
  aliases: z.set(z.string()), // unordered → a cty set
});

await new Provider()
  .resource("aws_s3_bucket", {
    schema: Bucket,
    async create(planned) {
      // planned: { name: string; arn: string; aliases: Set<string> }
      return { ...planned, arn: `arn:aws:s3:::${planned.name}` };
    },
  })
  .serve();
```

Schemas are [Zod](https://zod.dev) objects and **the type defines everything** —
structure, runtime validation, inferred handler types, *and* the Terraform
dispositions. Each disposition lives **on its field** via `.meta({ … })` (typed,
so a bad key is a compile error), not in out-of-band arrays:

| `.meta({ … })` | meaning |
|----------------|---------|
| `computed` | provider-filled, "known after apply" |
| `forceNew` | a change replaces the resource |
| `sensitive` | redacted in UI/logs |
| `writeOnly` | sent at apply, never persisted |
| `deprecated` | surfaces a deprecation notice |
| `block` | render as a nested HCL block (`name { … }`) |
| `set` | treat an array field as an unordered cty set |
| `searchKey` | a query input of a plural data source (`dataSourceList`) |

`create` is required; `read` / `update` / `delete` default to sensible no-ops.

The cty type is read **directly from the Zod type** (not via `z.toJSONSchema`),
so the *type* carries the meaning — the SDK's design law that equality should
fall out of a well-chosen type, not bespoke logic. **`z.set(T)` derives to a cty
`set` (unordered)**: model "order must not matter" as a set and Terraform's
structural diff is order-insensitive for free — no dedup/reordering code, the Zod
analog of `HashSet<T>` over `Vec<T>`. A scalar set round-trips as a JS `Set`
(marshaled to/from the JSON array on the wire); for an unordered collection of
**objects**, tag a `z.array(...)` field `.meta({ set: true })` (a JS `Set` can't
dedup objects by value, so `z.set(object)` is rejected). `z.array(T)` stays an
ordered `list`, `z.record(...)` a `map`.

`block` (or a `z.array(...).meta({ block: true })` for a repeatable one, plus
`set: true` for an unordered block) renders a field as a nested **block**
(`name { … }`) instead of an attribute. On the wire a block is just an
object/list/set, so your handlers see the field unchanged — see
[`examples/cloudflare-provider.ts`](examples/cloudflare-provider.ts), which puts
every disposition on its Zod field.

## Shipping the provider — one command, one file

Terraform/OpenTofu launches a provider by `exec`ing an executable named
`terraform-provider-<name>` and speaking the go-plugin protocol over its stdio.
That executable does **not** have to be a compiled binary — for Node it's just an
executable JavaScript file with a `#!/usr/bin/env node` shebang. The Rust core
does all the protocol work: `.serve()` runs the entire **go-plugin handshake**
(magic-cookie check, protocol negotiation, auto-mTLS, the unix socket, the
handshake line on stdout) and then serves gRPC until Terraform stops the process.
You only build the schema and handlers.

You don't assemble any of that by hand. Write plain TypeScript — no shebang, no
build wiring — and use the SDK's [tsdown](https://tsdown.dev) preset:

```ts
// tsdown.config.ts
import { defineProviderBundle } from "@tofu-sdk/core/tsdown";

export default defineProviderBundle({ entry: "src/provider.ts", name: "acme" });
```

```bash
npm i -D tsdown
npx tsdown
# -> dist/terraform-provider-acme
```

That single file is the whole provider: your code, every dependency (zod, your
cloud SDK), **and** the native addon are bundled in — the `.node` is base64-inlined
and `dlopen`ed from a temp file on first launch, so there's no sidecar to carry.
The shebang is generated and the executable bit is set for you. Drop the file
wherever Terraform expects it (or symlink it) and point a local Terraform at it
with a `dev_overrides` CLI config (see the repo's `AGENTS.md`).

`examples/cloudflare-provider.ts` + `examples/tsdown.config.mjs` are a complete
worked example; the whole flow is verified end-to-end against real OpenTofu.

> Prefer the preset over hand-rolled tooling. A plain `#!/usr/bin/env node`
> JavaScript file (or a `tsc`-built one — `tsc` preserves a shebang) also works if
> you want zero bundling, but then you ship the addon and `node_modules` alongside.
> Avoid a `#!/usr/bin/env -S npx tsx` entrypoint: `npx` can hit the network to
> resolve `tsx` at launch and stall the handshake.

## Build

This package compiles a Rust crate (`native/`) into a native addon via
`@napi-rs/cli`, then compiles the TypeScript wrapper:

```bash
pnpm install
pnpm build        # napi build (-> binding/) + tsc (-> dist/)
```

`pnpm build` must run inside the repo's Nix dev shell, where `cargo` and `PROTOC`
are available for the Rust build.

## Status

Early but functional. Resources (CRUD + `force_new` replacement + `import` +
`version`/`upgrade` state migrations + a resource `validate` hook), provider
configuration (`config`, with its own `validate` hook), **provider-defined
functions** (`function` — pure, positional, called as `provider::p::name(…)`),
both singular (`dataSource`) and plural (`dataSourceList`) data sources,
**ephemeral resources** (`ephemeral` — an `open`/`renew`/`close` lifecycle, never
persisted to state), HCL nested **blocks**, and single-file packaging (the
`@tofu-sdk/core/tsdown` preset) all work end-to-end against real OpenTofu — see
`test/e2e.test.mjs`.

Not yet wired up (the Rust core supports these; the Node binding doesn't expose
them yet): **list resources**, **state stores**, **resource identity**,
`modify_plan` / `move_state`, the handler `Ctx` (success-path warnings,
per-resource private state, cancellation) on resource/data-source handlers, and
semantic-equality normalization. Also: prebuilt multi-platform addons (the preset
inlines the addon for the platform you build on).
