#!/usr/bin/env node
// A minimal example provider authored with @tofu-sdk/core, used both as
// documentation and as the subject of the e2e test. In a real package you would
// `require("@tofu-sdk/core")`; in-repo we require the built output directly.
const { Provider } = require("../dist/index.js");

const ARN_PREFIX = "arn:aws:s3:::";

// Provider-level state, populated by `configure` and read by the handlers.
let region = "us-east-1";

new Provider()
  // Provider configuration: an optional `region`. `configure` runs once, before
  // any resource/data-source handler, and stashes the region in closure state.
  .config({
    schema: {
      region: { type: "string", optional: true },
    },
    async configure(config) {
      region = config.region || "us-east-1";
    },
  })
  // A managed resource: `name` is required and forces replacement when changed;
  // `arn` and `region` are computed (the latter from the configured provider).
  .resource("aws_s3_bucket", {
    schema: {
      name: { type: "string", required: true, forceNew: true },
      arn: { type: "string", computed: true },
      region: { type: "string", computed: true },
      tags: { type: ["map", "string"], optional: true },
    },
    async create(planned) {
      return { ...planned, arn: `${ARN_PREFIX}${planned.name}`, region };
    },
    async update(planned, _prior) {
      return { ...planned, arn: `${ARN_PREFIX}${planned.name}`, region };
    },
  })
  // A read-only data source: look a bucket up by name and compute its arn.
  .dataSource("aws_s3_bucket", {
    schema: {
      name: { type: "string", required: true },
      arn: { type: "string", computed: true },
    },
    async read(config) {
      return { ...config, arn: `${ARN_PREFIX}${config.name}` };
    },
  })
  .serve()
  .catch((err) => {
    console.error("provider failed:", err);
    process.exit(1);
  });
