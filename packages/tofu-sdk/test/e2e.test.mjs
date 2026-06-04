// End-to-end test for the JS provider path: drives a real `tofu` binary against
// the example provider (examples/aws-provider.cjs) via a dev_overrides workspace,
// exercising apply, computed attributes, a data source read, force_new
// replacement, and destroy. Requires `tofu`/`terraform` and a prior `pnpm build`
// (the repo's Nix dev shell provides the engine; `pnpm test` runs the build).

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  symlinkSync,
  rmSync,
  existsSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pkgRoot = resolve(here, "..");
const example = join(pkgRoot, "examples", "aws-provider.cjs");

/** Pick the engine, preferring OpenTofu. */
function engine() {
  for (const bin of ["tofu", "terraform"]) {
    if (spawnSync(bin, ["version"], { encoding: "utf8" }).status === 0) return bin;
  }
  throw new Error("these e2e tests require `tofu` or `terraform` on PATH");
}

/** A dev_overrides workspace pointing at the example provider. */
function workspace() {
  const dir = mkdtempSync(join(tmpdir(), "tofu-sdk-e2e-"));
  symlinkSync(example, join(dir, "terraform-provider-aws"));
  writeFileSync(
    join(dir, "tofurc"),
    `provider_installation {\n  dev_overrides { "example/aws" = "${dir}" }\n  direct {}\n}\n`,
  );
  const cfg = join(dir, "cfg");
  mkdirSync(cfg);
  return { dir, cfg, env: { ...process.env, TF_CLI_CONFIG_FILE: join(dir, "tofurc") } };
}

function run(bin, args, cfg, env) {
  return spawnSync(bin, args, { cwd: cfg, env, encoding: "utf8" });
}

test("example provider drives a real tofu lifecycle", () => {
  assert.ok(
    existsSync(join(pkgRoot, "dist", "index.js")),
    "build the package first (`pnpm build`)",
  );
  const bin = engine();
  const { dir, cfg, env } = workspace();
  try {
    writeFileSync(
      join(cfg, "main.tf"),
      `
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

variable "bucket_name" {
  type    = string
  default = "e2e-bucket"
}

resource "aws_s3_bucket" "test" {
  name = var.bucket_name
  tags = { env = "test" }
}

data "aws_s3_bucket" "looked_up" {
  name = "queried"
}

data "aws_s3_buckets" "many" {
  name = "team"
}

output "arn" {
  value = aws_s3_bucket.test.arn
}

output "region" {
  value = aws_s3_bucket.test.region
}

output "data_arn" {
  value = data.aws_s3_bucket.looked_up.arn
}

output "list_names" {
  value = data.aws_s3_buckets.many.results[*].name
}
`,
    );

    // Apply: the create handler computes the arn; the data source read computes
    // its own arn from the queried name.
    const apply = run(bin, ["apply", "-auto-approve"], cfg, env);
    assert.equal(apply.status, 0, `apply failed:\n${apply.stdout}\n${apply.stderr}`);

    const out = run(bin, ["output", "-json"], cfg, env);
    assert.equal(out.status, 0, out.stderr);
    const outputs = JSON.parse(out.stdout);
    assert.equal(outputs.arn.value, "arn:aws:s3:::e2e-bucket", "resource computed arn");
    assert.equal(outputs.region.value, "eu-west-1", "configured provider region reached the resource");
    assert.equal(outputs.data_arn.value, "arn:aws:s3:::queried", "data source computed arn");
    assert.deepEqual(
      outputs.list_names.value,
      ["team", "team-staging"],
      "plural data source returned the results list",
    );

    // Renaming the force_new `name` must plan a replacement.
    const plan = run(
      bin,
      ["plan", "-no-color", "-var", "bucket_name=renamed"],
      cfg,
      env,
    );
    assert.equal(plan.status, 0, plan.stderr);
    assert.match(
      plan.stdout,
      /forces replacement/,
      `changing name should force replacement:\n${plan.stdout}`,
    );

    // Apply the replacement and confirm the recomputed arn.
    const reapply = run(
      bin,
      ["apply", "-auto-approve", "-var", "bucket_name=renamed"],
      cfg,
      env,
    );
    assert.equal(reapply.status, 0, `reapply failed:\n${reapply.stderr}`);
    const out2 = JSON.parse(run(bin, ["output", "-json"], cfg, env).stdout);
    assert.equal(out2.arn.value, "arn:aws:s3:::renamed", "arn tracks the renamed bucket");

    const destroy = run(bin, ["destroy", "-auto-approve", "-var", "bucket_name=renamed"], cfg, env);
    assert.equal(destroy.status, 0, `destroy failed:\n${destroy.stderr}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
