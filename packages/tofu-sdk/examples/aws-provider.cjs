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
});

const BucketLookup = z.object({
  name: z.string(),
  arn: z.string(),
});

new Provider()
  // Provider configuration: an optional `region`, stashed for the handlers.
  .config({
    schema: z.object({ region: z.string().optional() }),
    async configure(config) {
      region = config.region || "us-east-1";
    },
  })
  // A managed resource: `name` forces replacement; `arn`/`region` are computed.
  .resource("aws_s3_bucket", {
    schema: Bucket,
    forceNew: ["name"],
    computed: ["arn", "region"],
    async create(planned) {
      return { ...planned, arn: `${ARN_PREFIX}${planned.name}`, region };
    },
    async update(planned, _prior) {
      return { ...planned, arn: `${ARN_PREFIX}${planned.name}`, region };
    },
    // Import an existing bucket by name (the id), recovering its computed state.
    async import(id) {
      return { name: id, arn: `${ARN_PREFIX}${id}`, region };
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
  .serve()
  .catch((err) => {
    console.error("provider failed:", err);
    process.exit(1);
  });
