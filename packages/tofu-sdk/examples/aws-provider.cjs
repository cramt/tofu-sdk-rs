#!/usr/bin/env node
// A minimal example provider authored with @tofu-sdk/core, used both as
// documentation and as the subject of the e2e test. Schemas are Zod; the
// computed/forceNew arrays are type-checked against the schema fields. In a real
// package you would `require("@tofu-sdk/core")`; in-repo we require the build.
const { z } = require("zod");
const { Provider } = require("../dist/index.js");

const ARN_PREFIX = "arn:aws:s3:::";

// Provider-level state, populated by `configure` and read by the handlers.
let region = "us-east-1";

const Bucket = z.object({
  name: z.string(),
  arn: z.string(),
  region: z.string(),
  tags: z.record(z.string(), z.string()).optional(),
  // A plain optional input; `modifyPlan` (below) force-replaces by rule when it
  // becomes "gold" — the analog of Rust's `Resource::modify_plan`.
  tier: z.string().optional(),
});

const BucketLookup = z.object({
  name: z.string(),
  arn: z.string(),
});

new Provider()
  // Provider configuration: an optional `region`, stashed for the handlers. The
  // `validate` hook rejects an obviously-bad region before configure runs.
  .config({
    schema: z.object({ region: z.string().optional() }),
    async configure(config) {
      region = config.region || "us-east-1";
    },
    validate(config) {
      if (config.region === "bad") {
        return [{ severity: "error", summary: "invalid region", attribute: ["region"] }];
      }
      return [];
    },
  })
  // A provider-defined function: `provider::aws::arn_for("my-bucket")`.
  .function("arn_for", {
    params: z.object({ name: z.string() }),
    returns: z.string(),
    summary: "Build an S3 ARN from a bucket name.",
    async call({ name }) {
      return `${ARN_PREFIX}${name}`;
    },
  })
  // A variadic function: `provider::aws::join("-", "a", "b", "c")`. Leading fixed
  // params (`separator`) plus a uniform trailing tail (`rest: string[]`).
  .functionVariadic("join", {
    params: z.object({ separator: z.string() }),
    variadic: z.string(),
    returns: z.string(),
    summary: "Join the variadic parts with the separator.",
    async call({ separator }, parts) {
      return parts.join(separator);
    },
  })
  // A managed resource: `name` forces replacement; `arn`/`region` are computed.
  .resource("aws_s3_bucket", {
    schema: Bucket,
    version: 1,
    forceNew: ["name"],
    computed: ["arn", "region"],
    async create(planned, ctx) {
      // `ctx` is the TS analog of Rust's `&mut Ctx`: success-path warnings,
      // per-resource private state (`ctx.private`/`ctx.setPrivate`), and
      // cancellation (`ctx.cancelled`/`ctx.signal`).
      ctx.warn("bucket provisioned", `created ${planned.name} in ${region}`);
      ctx.setPrivate(JSON.stringify({ createdRegion: region }));
      return { ...planned, arn: `${ARN_PREFIX}${planned.name}`, region };
    },
    async update(planned, _prior) {
      return { ...planned, arn: `${ARN_PREFIX}${planned.name}`, region };
    },
    // Import an existing bucket by name (the id), recovering its computed state.
    async import(id) {
      return { name: id, arn: `${ARN_PREFIX}${id}`, region };
    },
    // Migrate v0 state, which named the bucket `bucket` instead of `name`.
    async upgrade(_fromVersion, prior) {
      const name = prior?.name ?? prior?.bucket ?? "";
      return { name, arn: `${ARN_PREFIX}${name}`, region };
    },
    // Adjust the plan by rule: upgrading `tier` to "gold" forces replacement.
    async modifyPlan(prior, proposed) {
      if (prior && proposed.tier === "gold" && prior.tier !== "gold") {
        return { replace: [["tier"]] };
      }
    },
    // Reject invalid config early. `name` may be null (unset/unknown), so guard.
    validate(config) {
      const diagnostics = [];
      if (config.name && config.name !== config.name.toLowerCase()) {
        diagnostics.push({
          severity: "error",
          summary: "bucket name must be lowercase",
          attribute: ["name"],
        });
      }
      return diagnostics;
    },
  })
  // A singular data source: look a bucket up by name and compute its arn.
  .dataSource("aws_s3_bucket", {
    schema: BucketLookup,
    computed: ["arn"],
    async read(config) {
      return { ...config, arn: `${ARN_PREFIX}${config.name}` };
    },
  })
  // A plural data source: look buckets up by `name` -> a `results` list.
  .dataSourceList("aws_s3_buckets", {
    schema: BucketLookup,
    searchKeys: ["name"],
    async list(query) {
      return ["", "-staging"].map((suffix) => {
        const name = `${query.name}${suffix}`;
        return { name, arn: `${ARN_PREFIX}${name}` };
      });
    },
  })
  // An ephemeral resource: a short-lived session token, never written to state.
  // `open` mints it and stashes the role as the private handle so `renew`/`close`
  // (which receive only that handle) can act on it; `renewAt` asks the engine to
  // renew before the pretend TTL.
  .ephemeral("aws_session_token", {
    schema: z.object({ role: z.string(), token: z.string() }),
    computed: ["token"],
    sensitive: ["token"],
    async open(config) {
      return {
        result: { role: config.role, token: `tok-${config.role}-${region}` },
        private: config.role,
        renewAt: Date.now() + 5 * 60 * 1000,
      };
    },
    async renew(role) {
      // The handle is the role we stashed; re-arm the renewal window.
      return { renewAt: Date.now() + 5 * 60 * 1000, private: role };
    },
    async close(_role) {
      // A real provider would revoke the token here.
    },
  })
  .serve()
  .catch((err) => {
    console.error("provider failed:", err);
    process.exit(1);
  });
