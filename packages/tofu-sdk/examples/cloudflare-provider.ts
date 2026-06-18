#!/usr/bin/env -S npx tsx
// A pet example: a Cloudflare provider authored with @tofu-sdk/core that manages
// a single resource — `cloudflare_api_token` — by making *real* calls to the
// official `cloudflare` TypeScript SDK (`cf.user.tokens.{create,get,update,delete}`).
//
// It mirrors the shape of examples/aws-provider.cjs, but where that one fakes its
// backend, this one talks to Cloudflare for real. Applying it will mint (and
// destroy) actual API tokens on your account, so treat it as a live example.
//
// Run it against tofu/terraform via dev_overrides:
//
//   # from packages/tofu-sdk, after `pnpm build` (builds the native addon + dist):
//   pnpm add cloudflare tsx
//   export CLOUDFLARE_API_TOKEN=<a token with "User API Tokens: Edit" permission>
//
//   DIR=$(mktemp -d)
//   ln -s "$PWD/examples/cloudflare-provider.ts" "$DIR/terraform-provider-cloudflare"
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
//     policies = [{
//       effect            = "allow"
//       resources         = { "com.cloudflare.api.account.zone.*" = "*" }
//       permission_groups = [{ id = "<permission-group-id>" }]
//     }]
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
// groups. This is exactly Cloudflare's token-policy shape, expressed in Zod so
// the cty schema is derived for us. `effect` is "allow" | "deny" (kept as a
// plain string so the schema derivation stays a simple cty `string`).
const Policy = z.object({
  effect: z.string(),
  // e.g. { "com.cloudflare.api.account.zone.*": "*" } — scope glob -> "*".
  resources: z.record(z.string(), z.string()),
  permissionGroups: z.array(z.object({ id: z.string() })),
});

const ApiToken = z.object({
  // Inputs.
  name: z.string(),
  policies: z.array(Policy),

  // Computed, filled by Cloudflare on create.
  id: z.string(),
  value: z.string(), // the secret — only ever returned at creation time
  status: z.string(),
  issuedOn: z.string().optional(),
  modifiedOn: z.string().optional(),
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

/** Map our Zod policies onto the SDK's `policies` param shape. */
function toApiPolicies(policies: ApiToken["policies"]): TokenPolicies {
  return policies.map((p) => ({
    effect: p.effect as "allow" | "deny",
    resources: p.resources,
    permission_groups: p.permissionGroups.map((g) => ({ id: g.id })),
  }));
}

// --- the provider ----------------------------------------------------------

new Provider()
  .config({
    // `api_token` is optional in config; we fall back to CLOUDFLARE_API_TOKEN.
    schema: z.object({ apiToken: z.string().optional() }),
    async configure(config) {
      const apiToken = config.apiToken ?? process.env.CLOUDFLARE_API_TOKEN;
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
    // The secret value is computed and must never appear in plan output.
    computed: ["id", "value", "status", "issuedOn", "modifiedOn"],
    sensitive: ["value"],
    async create(planned) {
      const created = await client().user.tokens.create({
        name: planned.name,
        policies: toApiPolicies(planned.policies),
      });
      return {
        ...planned,
        id: created.id ?? "",
        value: created.value ?? "",
        status: created.status ?? "active",
        issuedOn: created.issued_on,
        modifiedOn: created.modified_on,
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
        modifiedOn: token.modified_on,
      };
    },
    async update(planned, prior) {
      const token = await client().user.tokens.update(prior.id, {
        name: planned.name,
        policies: toApiPolicies(planned.policies),
        status: prior.status as "active" | "disabled" | "expired",
      });
      return {
        ...planned,
        id: prior.id,
        value: prior.value, // unchanged; never re-returned
        status: token.status ?? prior.status,
        issuedOn: prior.issuedOn,
        modifiedOn: token.modified_on,
      };
    },
    async delete(prior) {
      await client().user.tokens.delete(prior.id);
    },
    // Reject obviously-broken config before planning.
    validate(config) {
      const diagnostics: Diagnostic[] = [];
      if (config.policies && config.policies.length === 0) {
        diagnostics.push({
          severity: "error",
          summary: "an API token needs at least one policy",
          attribute: ["policies"],
        });
      }
      return diagnostics;
    },
  })
  .serve()
  .catch((err) => {
    console.error("provider failed:", err);
    process.exit(1);
  });
