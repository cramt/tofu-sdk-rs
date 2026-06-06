// Core machinery for the iteration-sequence test harness.
//
// The harness drives a real `tofu`/`terraform` binary against the `example-fs`
// provider (../../crates/example-fs) via a `dev_overrides` workspace. For each
// test *configuration* it stands up one workspace — a persistent config dir and
// a fresh output dir — then applies an ordered sequence of *iterations* into it.
//
// Crucially, all iterations of a configuration share ONE working directory, so
// they share ONE local-backend state file (`terraform.tfstate`). That is the
// whole trick to "shared state" without S3: the local backend just keeps the
// state file in the cwd. Between iterations we swap the resource `.tf` files in
// place (keeping the harness-owned provider block) and re-apply, so resources
// added, changed, replaced, or removed across iterations exercise the full
// create / update / replace / delete lifecycle. The provider records each
// resource's attributes to `<output_dir>/<name>.json`, which is what we assert.

import { spawn, spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Repo root, derived from this file's location (harness/src → repo root). */
export const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

/** The built provider binary. `buildProvider()` produces it. */
export const providerBin = join(repoRoot, "target", "debug", "example-fs");

let engineBin: string | undefined;

/** The CLI binary, preferring OpenTofu. Memoized. Throws if neither is found. */
export function engine(): string {
  if (engineBin) return engineBin;
  for (const bin of ["tofu", "terraform"]) {
    if (spawnSync(bin, ["version"], { encoding: "utf8" }).status === 0) {
      engineBin = bin;
      return bin;
    }
  }
  throw new Error(
    "the harness requires `tofu` or `terraform` on PATH (enter the dev shell: `nix develop`)",
  );
}

/** The outcome of an external command. */
interface RunResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

/**
 * Run a command via async `spawn`, resolving with its captured output. Async
 * (not `spawnSync`) so a multi-second `apply` does not block the Node event loop
 * — otherwise the Vitest worker can't answer its heartbeat RPC and the run
 * reports a spurious timeout error even though every assertion passed.
 */
function run(
  cmd: string,
  args: string[],
  opts: { cwd: string; env?: NodeJS.ProcessEnv; inherit?: boolean },
): Promise<RunResult> {
  return new Promise((resolveRun) => {
    const child = spawn(cmd, args, {
      cwd: opts.cwd,
      env: opts.env,
      stdio: opts.inherit ? "inherit" : ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout?.on("data", (d) => (stdout += d));
    child.stderr?.on("data", (d) => (stderr += d));
    child.on("error", (err) => resolveRun({ status: -1, stdout, stderr: String(err) }));
    child.on("close", (code) => resolveRun({ status: code, stdout, stderr }));
  });
}

/** Build the example-fs provider once (cargo caches subsequent runs). */
export async function buildProvider(): Promise<void> {
  const res = await run("cargo", ["build", "-p", "example-fs"], {
    cwd: repoRoot,
    env: process.env,
    inherit: true,
  });
  if (res.status !== 0) {
    throw new Error("failed to build example-fs (is the nix dev shell active, with PROTOC set?)");
  }
}

/** A `dev_overrides` workspace: a persistent config dir + a fresh output dir. */
export interface Workspace {
  /** The temp root (cleaned up by `destroyWorkspace`). */
  readonly dir: string;
  /** The Terraform config dir (cwd for engine commands). */
  readonly cfg: string;
  /** Where the provider writes resource JSON files. */
  readonly outputDir: string;
  /** Env carrying `TF_CLI_CONFIG_FILE` pointed at the dev_overrides config. */
  readonly env: NodeJS.ProcessEnv;
}

/**
 * Stand up a workspace: symlink the provider, write the CLI config, and seed the
 * config dir with a harness-owned `_harness.tf` (required_providers + the
 * `output_dir` provider config). Only that file persists across iterations; the
 * iteration's own `.tf` files are swapped around it.
 */
export function createWorkspace(): Workspace {
  const dir = mkdtempSync(join(tmpdir(), "tofu-harness-"));
  symlinkSync(providerBin, join(dir, "terraform-provider-fs"));
  writeFileSync(
    join(dir, "tofurc"),
    `provider_installation {\n  dev_overrides { "example/fs" = "${dir}" }\n  direct {}\n}\n`,
  );

  const cfg = join(dir, "cfg");
  mkdirSync(cfg);
  const outputDir = join(dir, "output");
  mkdirSync(outputDir);

  // JSON.stringify yields a valid double-quoted HCL string (handles the path).
  writeFileSync(
    join(cfg, "_harness.tf"),
    `terraform {
  required_providers {
    fs = {
      source = "example/fs"
    }
  }
}

provider "fs" {
  output_dir = ${JSON.stringify(outputDir)}
}
`,
  );

  return { dir, cfg, outputDir, env: { ...process.env, TF_CLI_CONFIG_FILE: join(dir, "tofurc") } };
}

/** Remove every resource `.tf` from the config dir, keeping `_harness.tf`. */
function clearResourceFiles(cfg: string): void {
  for (const f of readdirSync(cfg)) {
    if (f.endsWith(".tf") && f !== "_harness.tf") rmSync(join(cfg, f));
  }
}

/**
 * Apply one iteration into the workspace: swap the iteration's `.tf` files in
 * (replacing the previous iteration's), then `apply -auto-approve`. State
 * carries over from the prior iteration via the shared `terraform.tfstate`.
 * Throws with the engine output on failure.
 */
export async function applyIteration(ws: Workspace, iterationDir: string): Promise<void> {
  clearResourceFiles(ws.cfg);
  for (const f of readdirSync(iterationDir)) {
    if (f.endsWith(".tf")) copyFileSync(join(iterationDir, f), join(ws.cfg, f));
  }
  const res = await run(engine(), ["apply", "-auto-approve", "-no-color"], {
    cwd: ws.cfg,
    env: ws.env,
  });
  if (res.status !== 0) {
    throw new Error(
      `apply failed for ${iterationDir}:\n--- stdout ---\n${res.stdout}\n--- stderr ---\n${res.stderr}`,
    );
  }
}

/** Destroy everything in the workspace and delete the temp dir. */
export async function destroyWorkspace(ws: Workspace): Promise<void> {
  await run(engine(), ["destroy", "-auto-approve", "-no-color"], { cwd: ws.cfg, env: ws.env });
  rmSync(ws.dir, { recursive: true, force: true });
}

/**
 * Read every `*.json` file in `dir` into `{ filename: parsedContent }`. Missing
 * dirs yield `{}`. Parsing decouples the assertion from key ordering in the
 * written files.
 */
export function readJsonDir(dir: string): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  if (!existsSync(dir)) return out;
  for (const f of readdirSync(dir)) {
    if (f.endsWith(".json")) {
      out[f] = JSON.parse(readFileSync(join(dir, f), "utf8"));
    }
  }
  return out;
}

/** One step in a configuration's sequence. */
export interface Iteration {
  /** The folder name (e.g. `"1"`), used as the test title and for ordering. */
  readonly name: string;
  /** Absolute path to the iteration folder (holds `*.tf` + `expected/`). */
  readonly dir: string;
}

/** A named test configuration: an ordered list of iterations. */
export interface Config {
  readonly name: string;
  readonly dir: string;
  readonly iterations: Iteration[];
}

/**
 * Discover configurations under `configsRoot`. Layout:
 * `configsRoot/<config>/<iteration>/{*.tf, expected/*.json}`. Iteration folders
 * sort numerically (`1` < `2` < `10`), so applies run in the intended order.
 */
export function discoverConfigs(configsRoot: string): Config[] {
  return readdirSync(configsRoot, { withFileTypes: true })
    .filter((d) => d.isDirectory())
    .map((d) => {
      const dir = join(configsRoot, d.name);
      const iterations = readdirSync(dir, { withFileTypes: true })
        .filter((e) => e.isDirectory())
        .map((e) => ({ name: e.name, dir: join(dir, e.name) }))
        .sort((a, b) => Number(a.name) - Number(b.name) || a.name.localeCompare(b.name));
      return { name: d.name, dir, iterations };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
}
