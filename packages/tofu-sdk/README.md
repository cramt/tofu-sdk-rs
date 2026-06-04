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
`forceNew`, `sensitive` — are arrays of field names that are **type-checked
against the schema** (a typo is a compile error). `create` is required;
`read` / `update` / `delete` default to sensible no-ops. `forceNew` drives
replacement in the planning engine; `computed` attributes are filled by your
handlers and surface as "known after apply".

## The provider binary

Terraform launches a provider as an executable named `terraform-provider-<name>`.
Make your entrypoint that executable — a Node script with a shebang works:

```js
#!/usr/bin/env node
const { Provider } = require("@tofu-sdk/core");
new Provider().resource(/* … */).serve().catch((e) => {
  console.error(e);
  process.exit(1);
});
```

Point Terraform/OpenTofu at it during development with a `dev_overrides` CLI
config (see the repo's `AGENTS.md`).

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

Early but functional. Resources (CRUD + `force_new` replacement + `import`),
provider configuration (`config`), and both singular (`dataSource`) and plural
(`dataSourceList`) data sources work end-to-end against real OpenTofu — see
`test/e2e.test.mjs`. Not yet wired up: config validation hooks, prebuilt
multi-platform binaries, HCL block syntax, and a generated
`terraform-provider-*` launcher (you write the shebang entry yourself for now).
