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

// A predecessor resource type that named buckets by `label`. It exists only so a
// `moved {}` block can migrate its state into `aws_s3_bucket` across types.
const LegacyBucket = z.object({
  label: z.string(),
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
    // The bucket's stable identity is its name (import-by-identity, tracking).
    identity: ["name"],
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
    // Migrate state from the predecessor `aws_legacy_bucket` (a cross-type
    // `moved {}`): its `label` becomes our `name`, and the computed fields are
    // recovered. The source state is untyped (a foreign schema), so we read it
    // defensively.
    async moveState(sourceTypeName, sourceState) {
      if (sourceTypeName !== "aws_legacy_bucket") {
        throw new Error(`cannot move state from "${sourceTypeName}"`);
      }
      const name = sourceState?.label ?? "";
      return { name, arn: `${ARN_PREFIX}${name}`, region };
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
  // The predecessor resource type. It only needs to exist so its stored state
  // can be migrated into `aws_s3_bucket` via a cross-type `moved {}` block (see
  // the `aws_s3_bucket` `moveState` handler above).
  .resource("aws_legacy_bucket", {
    schema: LegacyBucket,
    async create(planned) {
      return planned;
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
  // A list resource: enumerate existing buckets matching a query prefix. Shares
  // the `aws_s3_bucket` type name with the managed resource; each result is
  // projected to the resource's `name` identity. Driven by `terraform query`.
  .listResource("aws_s3_bucket", {
    schema: Bucket,
    forceNew: ["name"],
    computed: ["arn", "region"],
    identity: ["name"],
    config: z.object({ prefix: z.string().optional() }),
    async list(query) {
      const prefix = query.prefix ?? "team";
      return [`${prefix}-a`, `${prefix}-b`].map((name) => ({
        displayName: name,
        resource: { name, arn: `${ARN_PREFIX}${name}`, region },
      }));
    },
  })
  // A state store: an in-memory Terraform backend keyed by state id. A real one
  // would talk to S3/GCS/etc.; this keeps state in a Map for the duration of the
  // provider process.
  .stateStore("inmem", {
    schema: z.object({}),
    async configure() {
      const states = new Map();
      let counter = 0;
      return {
        async readState(id) {
          return states.get(id) ?? null;
        },
        async writeState(id, data) {
          states.set(id, data);
        },
        async lock() {
          return `lock-${++counter}`;
        },
        async unlock() {},
        async states() {
          return [...states.keys()];
        },
        async deleteState(id) {
          states.delete(id);
        },
      };
    },
  })
  .serve()
  .catch((err) => {
    console.error("provider failed:", err);
    process.exit(1);
  });
