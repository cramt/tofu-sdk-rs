// End-to-end tests for the protocol surfaces OpenTofu 1.12 can't drive but
// HashiCorp Terraform 1.15 can: **list resources** (`terraform query`) and
// **state stores** (published in `providers schema -json`). These run against the
// example provider (examples/aws-provider.cjs) through a dev_overrides workspace,
// and require `terraform` >= 1.14 on PATH (the Nix dev shell provides 1.15) plus a
// prior `pnpm build`. If terraform is absent/too old, the tests skip.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, symlinkSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pkgRoot = resolve(here, "..");
const example = join(pkgRoot, "examples", "aws-provider.cjs");

/** Locate a Terraform >= 1.14 (list resources / `terraform query`), or null. */
function terraform() {
  const probe = spawnSync("terraform", ["version", "-json"], { encoding: "utf8" });
  if (probe.status !== 0) return null;
  try {
    const version = JSON.parse(probe.stdout).terraform_version ?? "";
    const [major, minor] = version.split(".").map(Number);
    if (major > 1 || (major === 1 && minor >= 14)) return "terraform";
  } catch {
    /* fall through */
  }
  return null;
}

/** A dev_overrides workspace pointing at the example provider. */
function workspace() {
  const dir = mkdtempSync(join(tmpdir(), "tofu-sdk-tf-"));
  symlinkSync(example, join(dir, "terraform-provider-aws"));
  writeFileSync(
    join(dir, "tofurc"),
    `provider_installation {\n  dev_overrides { "example/aws" = "${dir}" }\n  direct {}\n}\n`,
  );
  const cfg = join(dir, "cfg");
  mkdirSync(cfg);
  return { dir, cfg, env: { ...process.env, TF_CLI_CONFIG_FILE: join(dir, "tofurc") } };
}

const MAIN_TF = `
terraform {
  required_providers {
    aws = {
      source = "example/aws"
    }
  }
}

provider "aws" {
  region = "eu-west-1"
}
`;

function run(bin, args, cfg, env) {
  return spawnSync(bin, args, { cwd: cfg, env, encoding: "utf8" });
}

test("Terraform 1.15: list resource + state store appear in the schema", { skip: !terraform() }, () => {
  const bin = terraform();
  const { dir, cfg, env } = workspace();
  try {
    writeFileSync(join(cfg, "main.tf"), MAIN_TF);
    const out = run(bin, ["providers", "schema", "-json"], cfg, env);
    assert.equal(out.status, 0, `providers schema failed:\n${out.stdout}\n${out.stderr}`);
    const schema = JSON.parse(out.stdout);
    const provider = Object.entries(schema.provider_schemas).find(([k]) =>
      k.endsWith("example/aws"),
    )[1];

    // The list resource publishes its `list {}` query block.
    const list = provider.list_resource_schemas?.aws_s3_bucket;
    assert.ok(list, "the list resource is present in list_resource_schemas");
    assert.equal(list.block.attributes.prefix.optional, true, "the query block exposes `prefix`");

    // Its identity is published under the shared resource identity schema (the
    // list resource projects each result to the managed resource's identity).
    const identity = provider.resource_identity_schemas?.aws_s3_bucket;
    assert.ok(identity, "the resource identity schema is published");
    assert.equal(
      identity.attributes.name.required_for_import,
      true,
      "the `name` identity attribute is required for import",
    );

    // The state store publishes its config block.
    const store = provider.state_store_schemas?.inmem;
    assert.ok(store, "the state store is present in state_store_schemas");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("Terraform 1.15: terraform query lists existing instances", { skip: !terraform() }, () => {
  const bin = terraform();
  const { dir, cfg, env } = workspace();
  try {
    writeFileSync(join(cfg, "main.tf"), MAIN_TF);
    // A `.tfquery.hcl` file with a `list {}` block drives the list resource. The
    // provider's `list` handler returns two buckets keyed off the prefix; each is
    // projected to its `name` identity.
    writeFileSync(
      join(cfg, "list.tfquery.hcl"),
      `
list "aws_s3_bucket" "all" {
  provider = aws
  config {
    prefix = "team"
  }
}
`,
    );

    const query = run(bin, ["query"], cfg, env);
    assert.equal(query.status, 0, `terraform query failed:\n${query.stdout}\n${query.stderr}`);
    // The handler ran, the items streamed back, and identity projected — the
    // output lists both instances with their `name` identity.
    assert.match(query.stdout, /name=team-a/, `expected team-a in query output:\n${query.stdout}`);
    assert.match(query.stdout, /name=team-b/, `expected team-b in query output:\n${query.stdout}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
