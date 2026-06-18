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
  name: z.string(),
  arn: z.string(),
});

await new Provider()
  .resource("aws_s3_bucket", {
    schema: Bucket,
    forceNew: ["name"], // only "name" | "arn" type-checks here
    computed: ["arn"],
    async create(planned) {
      // planned is typed { name: string; arn: string }
      return { ...planned, arn: `arn:aws:s3:::${planned.name}` };
    },
  })
  .serve();
```

Schemas are [Zod](https://zod.dev) objects: you get runtime validation and
inferred handler types for free, and the cty schema Terraform needs is derived
from them. The Terraform-only dispositions Zod can't express — `computed`,
`forceNew`, `sensitive`, `blocks` — are arrays of field names that are
**type-checked against the schema** (a typo is a compile error). `create` is
required; `read` / `update` / `delete` default to sensible no-ops. `forceNew`
drives replacement in the planning engine; `computed` attributes are filled by
your handlers and surface as "known after apply".

`blocks` renders a field as a nested **block** (`name { … }`) instead of an
object/list attribute (`name = …`): name an object field for a single block, or
an array-of-objects field for a repeatable one. On the wire a block is just an
object/list, so your handlers see the field unchanged — see
[`examples/cloudflare-provider.ts`](examples/cloudflare-provider.ts) for a
repeatable `policy { … }` block.

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
`version`/`upgrade` state migrations + a `validate` config hook), provider
configuration (`config`), both singular (`dataSource`) and plural
(`dataSourceList`) data sources, HCL nested **blocks** (the `blocks` disposition),
and single-file packaging (the `@tofu-sdk/core/tsdown` preset) all work end-to-end
against real OpenTofu — see `test/e2e.test.mjs`. Not yet wired up: prebuilt
multi-platform addons (the preset inlines the addon for the platform you build
on, so cross-compiling a provider for other OSes/arches isn't covered yet).
