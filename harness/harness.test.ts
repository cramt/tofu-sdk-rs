// The harness suite: for each configuration under `configs/`, apply its
// iterations in order into one shared-state workspace and assert the set of JSON
// files the provider wrote after each step.
//
// `describe.each` fans out over configurations; `it.each` runs the iterations of
// a configuration in order. Vitest runs tests within a file sequentially and in
// declaration order, so the iterations share the workspace (created once in
// `beforeAll`) and its single `terraform.tfstate` — exactly the shared-state
// sequence we want.

import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { afterAll, beforeAll, describe, expect, it } from "vitest";

import {
  type Workspace,
  applyIteration,
  createWorkspace,
  destroyWorkspace,
  discoverConfigs,
  readJsonDir,
} from "./src/harness";

const here = dirname(fileURLToPath(import.meta.url));
const configs = discoverConfigs(join(here, "configs"));

describe.each(configs)("config $name", (config) => {
  let ws: Workspace;

  beforeAll(() => {
    ws = createWorkspace();
  });

  afterAll(async () => {
    if (ws) await destroyWorkspace(ws);
  });

  it.each(config.iterations)("iteration $name", async (iteration) => {
    await applyIteration(ws, iteration.dir);

    const actual = readJsonDir(ws.outputDir);
    const expected = readJsonDir(join(iteration.dir, "expected"));

    expect(actual).toEqual(expected);
  });
});
