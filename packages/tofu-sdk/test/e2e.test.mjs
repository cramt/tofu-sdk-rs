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

variable "tier" {
  type    = string
  default = "silver"
}

resource "aws_s3_bucket" "test" {
  name = var.bucket_name
  tier = var.tier
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

output "fn_arn" {
  value = provider::aws::arn_for("fn-bucket")
}

output "fn_join" {
  value = provider::aws::join("-", "a", "b", "c")
}
`,
    );

    // Apply: the create handler computes the arn; the data source read computes
    // its own arn from the queried name.
    const apply = run(bin, ["apply", "-auto-approve"], cfg, env);
    assert.equal(apply.status, 0, `apply failed:\n${apply.stdout}\n${apply.stderr}`);
    // The create handler emitted a success-path warning through its `ctx`.
    assert.match(
      `${apply.stdout}\n${apply.stderr}`,
      /bucket provisioned/,
      "ctx.warn surfaced a provider warning on apply",
    );

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
    assert.equal(
      outputs.fn_arn.value,
      "arn:aws:s3:::fn-bucket",
      "provider-defined function returned the computed arn",
    );
    assert.equal(
      outputs.fn_join.value,
      "a-b-c",
      "variadic function joined the trailing args",
    );

    // modifyPlan force-replace-by-rule: upgrading `tier` to "gold" must replace.
    const tierPlan = run(bin, ["plan", "-no-color", "-var", "tier=gold"], cfg, env);
    assert.equal(tierPlan.status, 0, tierPlan.stderr);
    assert.match(
      tierPlan.stdout,
      /forces replacement/,
      `modifyPlan should force replacement on tier=gold:\n${tierPlan.stdout}`,
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

test("upgrades v0 state to the current schema", () => {
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

resource "aws_s3_bucket" "test" {
  name = "legacy"
}
`,
    );
    // Pre-seed state at schema_version 0, where the bucket was named by a
    // `bucket` attribute (the current schema uses `name`).
    const priorState = {
      version: 4,
      terraform_version: "1.12.0",
      serial: 1,
      lineage: "00000000-0000-0000-0000-000000000000",
      outputs: {},
      resources: [
        {
          mode: "managed",
          type: "aws_s3_bucket",
          name: "test",
          provider: 'provider["registry.terraform.io/example/aws"]',
          instances: [{ schema_version: 0, attributes: { bucket: "legacy" } }],
        },
      ],
    };
    writeFileSync(join(cfg, "terraform.tfstate"), JSON.stringify(priorState));

    // A refresh reads prior state; the stored version (0) < current (1) triggers
    // UpgradeResourceState, which runs the resource's `upgrade` migration.
    const refresh = run(bin, ["apply", "-refresh-only", "-auto-approve"], cfg, env);
    assert.equal(refresh.status, 0, `refresh failed:\n${refresh.stdout}\n${refresh.stderr}`);

    const show = run(bin, ["show", "-json"], cfg, env);
    assert.equal(show.status, 0, show.stderr);
    const state = JSON.parse(show.stdout);
    const bucket = state.values.root_module.resources.find(
      (r) => r.type === "aws_s3_bucket",
    ).values;
    assert.equal(bucket.name, "legacy", "v0 `bucket` migrated to `name`");
    assert.equal(bucket.arn, "arn:aws:s3:::legacy", "arn recomputed after upgrade");

    run(bin, ["destroy", "-auto-approve"], cfg, env);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("rejects invalid config via a validate hook", () => {
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

resource "aws_s3_bucket" "test" {
  name = "NotLowercase"
}
`,
    );
    // Planning runs ValidateResourceConfig; the validate hook rejects the name.
    const plan = run(bin, ["plan", "-no-color"], cfg, env);
    assert.notEqual(plan.status, 0, "plan should fail validation");
    assert.match(
      plan.stdout + plan.stderr,
      /bucket name must be lowercase/,
      `expected the validation diagnostic:\n${plan.stdout}\n${plan.stderr}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("imports an existing resource by id", () => {
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

resource "aws_s3_bucket" "test" {
  name = "imported"
}
`,
    );

    const imp = run(bin, ["import", "aws_s3_bucket.test", "imported"], cfg, env);
    assert.equal(imp.status, 0, `import failed:\n${imp.stdout}\n${imp.stderr}`);

    const show = run(bin, ["state", "show", "aws_s3_bucket.test"], cfg, env);
    assert.equal(show.status, 0, show.stderr);
    assert.match(
      show.stdout,
      /arn\s+= "arn:aws:s3:::imported"/,
      `imported state should carry the computed arn:\n${show.stdout}`,
    );
    assert.match(
      show.stdout,
      /region\s+= "eu-west-1"/,
      "the configured provider region should apply during import",
    );

    run(bin, ["destroy", "-auto-approve"], cfg, env);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("exposes the ephemeral resource in the engine's schema", () => {
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
`,
    );
    // The engine launches the JS provider and reports its schema; the ephemeral
    // resource must surface in its own `ephemeral_resource_schemas` map with a
    // required `role` input and a computed, sensitive `token` result.
    const out = run(bin, ["providers", "schema", "-json"], cfg, env);
    assert.equal(out.status, 0, `providers schema failed:\n${out.stdout}\n${out.stderr}`);
    const schema = JSON.parse(out.stdout);
    const provider = Object.entries(schema.provider_schemas).find(([k]) =>
      k.endsWith("example/aws"),
    )[1];
    const eph = provider.ephemeral_resource_schemas?.aws_session_token;
    assert.ok(eph, "the ephemeral resource is present in the schema");
    const attrs = eph.block.attributes;
    assert.equal(attrs.role.required, true, "role is a required ephemeral input");
    assert.equal(attrs.token.computed, true, "token is a computed ephemeral result");
    assert.equal(attrs.token.sensitive, true, "token is sensitive");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("opens the ephemeral resource and its minted value reaches HCL", () => {
  const bin = engine();
  const { dir, cfg, env } = workspace();
  try {
    // A `check` block's assertion references the ephemeral token: the engine must
    // Open the resource (running the JS `open` handler) to evaluate it, then
    // Close it — without persisting the value. The assertion proves the minted
    // value round-tripped through the napi marshalling correctly; a failing
    // condition surfaces as `token not minted` in the output.
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

ephemeral "aws_session_token" "s" {
  role = "admin"
}

check "minted" {
  assert {
    condition     = startswith(ephemeral.aws_session_token.s.token, "tok-admin-")
    error_message = "token not minted"
  }
}
`,
    );

    const apply = run(bin, ["apply", "-auto-approve"], cfg, env);
    assert.equal(apply.status, 0, `apply failed:\n${apply.stdout}\n${apply.stderr}`);
    // The engine logs the Open/Close lifecycle, and the check did not fire.
    assert.match(apply.stdout, /Open complete/, "the ephemeral resource was opened");
    assert.doesNotMatch(
      apply.stdout + apply.stderr,
      /token not minted/,
      "the minted token value reached HCL through the marshalling",
    );

    const destroy = run(bin, ["destroy", "-auto-approve"], cfg, env);
    assert.equal(destroy.status, 0, `destroy failed:\n${destroy.stdout}\n${destroy.stderr}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
