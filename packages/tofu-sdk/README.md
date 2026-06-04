# @tofu-sdk/core

Write Terraform/OpenTofu providers in TypeScript, backed by a Rust `tfplugin6`
core. You describe each resource and data source with a schema and async
handlers; the native addon runs the real plugin server (go-plugin handshake,
auto-mTLS, msgpack codec, planning) and drives your handlers with decoded
values. No protocol, gRPC, or `cty` encoding to deal with.

```ts
import { Provider } from "@tofu-sdk/core";

interface Bucket {
  name: string;
  arn: string;
}

await new Provider()
  .resource<Bucket>("aws_s3_bucket", {
    schema: {
      name: { type: "string", required: true, forceNew: true },
      arn: { type: "string", computed: true },
    },
    async create(planned) {
      return { ...planned, arn: `arn:aws:s3:::${planned.name}` };
    },
  })
  .serve();
```

`create` is required; `read` / `update` / `delete` are optional (sensible
no-ops by default). `forceNew` attributes drive replacement in the Rust planning
engine. `computed` attributes are filled by your handlers and surface as "known
after apply" during planning.

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

Early but functional. Resources (CRUD + `force_new` replacement), provider
configuration (`config`), and both singular (`dataSource`) and plural
(`dataSourceList`) data sources work end-to-end against real OpenTofu — see
`test/e2e.test.mjs`. Not yet wired up: prebuilt multi-platform binaries, nested
blocks, and a generated `terraform-provider-*` launcher (you write the shebang
entry yourself for now).
