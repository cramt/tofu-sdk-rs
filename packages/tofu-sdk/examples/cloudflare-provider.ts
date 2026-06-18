// A pet example: a Cloudflare provider authored with @tofu-sdk/core that manages
// a single resource — `cloudflare_api_token` — by making *real* calls to the
// official `cloudflare` TypeScript SDK (`cf.user.tokens.{create,get,update,delete}`).
//
// It mirrors the shape of examples/aws-provider.cjs, but where that one fakes its
// backend, this one talks to Cloudflare for real. Applying it will mint (and
// destroy) actual API tokens on your account, so treat it as a live example.
//
// Two things to note in the schema:
//   * `policy` is a repeatable nested **block** (HCL `policy { … }`), not a
//     `policies = [{ … }]` attribute — declared via `blocks: ["policy"]`.
//   * permission groups are given **by name** (`permission_groups = ["DNS Write"]`);
//     the create/update handlers resolve those names to Cloudflare's group IDs.
//
// This is plain TypeScript — no shebang, no build wiring. Bundle it into a single
// self-contained `terraform-provider-cloudflare` executable with the SDK's tsdown
// preset (see examples/tsdown.config.mjs and the README's "Shipping the provider"):
//
//   # from packages/tofu-sdk, after `pnpm build` (native addon + dist):
//   npx tsdown --config examples/tsdown.config.mjs
//   # -> examples/dist/terraform-provider-cloudflare  (one file, native addon inlined)
//
//   export CLOUDFLARE_API_TOKEN=<a token with "User API Tokens: Edit" permission>
//   DIR=$(mktemp -d)
//   ln -s "$PWD/examples/dist/terraform-provider-cloudflare" \
//         "$DIR/terraform-provider-cloudflare"
//   cat > "$DIR/tofurc" <<EOF
//   provider_installation {
//     dev_overrides { "example/cloudflare" = "$DIR" }
//     direct {}
//   }
//   EOF
//   # ...write a main.tf (below), then: TF_CLI_CONFIG_FILE="$DIR/tofurc" tofu apply
//
//   terraform {
//     required_providers { cloudflare = { source = "example/cloudflare" } }
//   }
//   provider "cloudflare" {}
//
//   resource "cloudflare_api_token" "ci" {
//     name = "ci-deploy"
//     policy {
//       effect            = "allow"
//       resources         = { "com.cloudflare.api.account.zone.*" = "*" }
//       permission_groups = ["DNS Write", "Zone Read"]
//     }
//   }
//
//   output "token" {
//     value     = cloudflare_api_token.ci.value
//     sensitive = true
//   }
//
// In a real package you would `import { Provider } from "@tofu-sdk/core"`; in-repo
// we import the TypeScript source directly so the example type-checks against it.
import Cloudflare from "cloudflare";
import { z } from "zod";

import { Provider, type Diagnostic } from "../ts/index";

// --- the model -------------------------------------------------------------

// One policy = an effect over a set of resources, granted a set of permission
// groups *by name*. Rendered as a repeatable `policy { … }` block (see
// `blocks: ["policy"]` below). `z.enum` derives to a cty `string` (a JSON-Schema
// `{ type: "string", enum: [...] }`) and gives the handlers the literal union for
// free — so no casts when calling the Cloudflare SDK.
const Policy = z.object({
  effect: z.enum(["allow", "deny"]),
  // e.g. { "com.cloudflare.api.account.zone.*": "*" } — scope glob -> "*".
  resources: z.record(z.string(), z.string()),
  // Permission-group names, e.g. ["DNS Write", "Zone Read"]; resolved to IDs.
  permission_groups: z.array(z.string()),
});

const ApiToken = z.object({
  // Inputs.
  name: z.string(),
  policy: z.array(Policy),

  // Computed, filled by Cloudflare on create.
  id: z.string(),
  value: z.string(), // the secret — only ever returned at creation time
  status: z.enum(["active", "disabled", "expired"]),
  issued_on: z.string().optional(),
  modified_on: z.string().optional(),
});

type ApiToken = z.infer<typeof ApiToken>;

// --- the Cloudflare client -------------------------------------------------

// Stashed by `configure`; the handlers read it. Constructed only when the host
// configures the provider (not when it merely fetches the schema), so a token is
// required to apply but not to load.
let cf: Cloudflare | null = null;

function client(): Cloudflare {
  if (!cf) throw new Error("cloudflare provider is not configured");
  return cf;
}

/** Cloudflare's create-token policy type, borrowed without naming its namespace. */
type TokenPolicies = Parameters<Cloudflare["user"]["tokens"]["create"]>[0]["policies"];

// Permission groups are stable per account; fetch the catalog once and cache the
// name -> id map for the life of the process.
let permissionGroupIds: Map<string, string> | null = null;

async function permissionGroupCatalog(): Promise<Map<string, string>> {
  if (permissionGroupIds) return permissionGroupIds;
  const byName = new Map<string, string>();
  // Auto-paginates; each group has an `id` and a human-readable `name`.
  for await (const group of client().user.tokens.permissionGroups.list()) {
    if (group.name && group.id) byName.set(group.name, group.id);
  }
  permissionGroupIds = byName;
  return byName;
}

/** Map our by-name Zod policies onto the SDK's `policies` param (groups by id). */
async function toApiPolicies(policies: ApiToken["policy"]): Promise<TokenPolicies> {
  const catalog = await permissionGroupCatalog();
  return policies.map((p) => ({
    effect: p.effect,
    resources: p.resources,
    permission_groups: p.permission_groups.map((name) => {
      const id = catalog.get(name);
      if (!id) throw new Error(`unknown permission group: "${name}"`);
      return { id };
    }),
  }));
}

// --- the provider ----------------------------------------------------------

new Provider()
  .config({
    // `api_token` is optional in config; we fall back to CLOUDFLARE_API_TOKEN.
    schema: z.object({ api_token: z.string().optional() }),
    async configure(config) {
      const apiToken = config.api_token ?? process.env.CLOUDFLARE_API_TOKEN;
      if (!apiToken) {
        throw new Error(
          "set `api_token` in the provider block or CLOUDFLARE_API_TOKEN in the env",
        );
      }
      cf = new Cloudflare({ apiToken });
    },
  })
  .resource("cloudflare_api_token", {
    schema: ApiToken,
    // `policy` is a repeatable HCL block, not a `policy = [...]` attribute.
    blocks: ["policy"],
    // The secret value is computed and must never appear in plan output.
    computed: ["id", "value", "status", "issued_on", "modified_on"],
    sensitive: ["value"],
    async create(planned) {
      const created = await client().user.tokens.create({
        name: planned.name,
        policies: await toApiPolicies(planned.policy),
      });
      return {
        ...planned,
        id: created.id ?? "",
        value: created.value ?? "",
        status: created.status ?? "active",
        issued_on: created.issued_on,
        modified_on: created.modified_on,
      };
    },
    async read(current) {
      // `get` never returns the secret, so we carry `value` over from state.
      const token = await client().user.tokens.get(current.id);
      if (!token) return null;
      return {
        ...current,
        name: token.name ?? current.name,
        status: token.status ?? current.status,
        modified_on: token.modified_on,
      };
    },
    async update(planned, prior) {
      const token = await client().user.tokens.update(prior.id, {
        name: planned.name,
        policies: await toApiPolicies(planned.policy),
        status: prior.status,
      });
      return {
        ...planned,
        id: prior.id,
        value: prior.value, // unchanged; never re-returned
        status: token.status ?? prior.status,
        issued_on: prior.issued_on,
        modified_on: token.modified_on,
      };
    },
    async delete(prior) {
      await client().user.tokens.delete(prior.id);
    },
    // Reject obviously-broken config before planning.
    validate(config) {
      const diagnostics: Diagnostic[] = [];
      if (config.policy && config.policy.length === 0) {
        diagnostics.push({
          severity: "error",
          summary: "an API token needs at least one policy",
          attribute: ["policy"],
        });
      }
      return diagnostics;
    },
  })
  // Hand off to the Rust core: `.serve()` performs the go-plugin handshake
  // (magic-cookie check, protocol negotiation, auto-mTLS, the handshake line on
  // stdout) and then runs the gRPC server until Terraform stops the process.
  // Nothing protocol-related lives in this file.
  .serve()
  .catch((err) => {
    console.error("provider failed:", err);
    process.exit(1);
  });
