/**
 * Default [tsdown](https://tsdown.dev) bundling preset for Terraform providers.
 *
 * Authors write a plain-TypeScript provider — no shebang, no build wiring — and a
 * three-line `tsdown.config.ts`:
 *
 * ```ts
 * import { defineProviderBundle } from "@tofu-sdk/core/tsdown";
 *
 * export default defineProviderBundle({ entry: "src/provider.ts", name: "cloudflare" });
 * ```
 *
 * `npx tsdown` then emits a **single** self-contained executable,
 * `terraform-provider-<name>` — the shebang is generated and the file is marked
 * executable. There is no sidecar to ship: the native addon (`*.node`) is
 * base64-inlined into the bundle and, on first launch, written to the OS temp
 * directory and `process.dlopen`ed (cached per addon hash, so it materializes
 * once). All of this — bundling the author's deps, embedding the right platform
 * binary, generating the shebang and the executable bit — lives in here, not in
 * the author's config.
 *
 * Caveat: the addon is `dlopen`ed from the OS temp directory, so a `noexec` mount
 * on `$TMPDIR` will prevent it from loading (rare, but set `TMPDIR` to an
 * exec-capable path if you hit it).
 */

import { createHash } from "node:crypto";
import { chmodSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";

/** Options for {@link defineProviderBundle}. */
export interface ProviderBundleOptions {
  /** Entry module: the file that builds a `Provider` and calls `.serve()`. */
  entry: string;
  /** Provider short name; the output executable is `terraform-provider-<name>`. */
  name: string;
  /** Output directory (default `"dist"`). */
  outDir?: string;
}

const SHEBANG = "#!/usr/bin/env node\n";
// A virtual module id (rollup `\0` convention) standing in for the napi loader.
const NATIVE_MODULE = "\0tofu-sdk-native-addon";

/**
 * Generate the CommonJS module that inlines the addon `bytes` (as base64) and,
 * at runtime, materializes them to a per-hash temp file and `dlopen`s it.
 */
function inlineAddonModule(bytes: Buffer): string {
  const b64 = bytes.toString("base64");
  const hash = createHash("sha256").update(bytes).digest("hex").slice(0, 16);
  return [
    `const { existsSync, mkdtempSync, writeFileSync, renameSync } = require("node:fs");`,
    `const { join } = require("node:path");`,
    `const { tmpdir } = require("node:os");`,
    `const target = join(tmpdir(), "tofu-sdk-addon-${hash}.node");`,
    `if (!existsSync(target)) {`,
    `  const scratch = join(mkdtempSync(join(tmpdir(), "tofu-sdk-addon-")), "addon.node");`,
    `  writeFileSync(scratch, Buffer.from(${JSON.stringify(b64)}, "base64"));`,
    `  try { renameSync(scratch, target); } catch { /* lost the race; another process wrote it */ }`,
    `}`,
    `const mod = { exports: {} };`,
    `process.dlopen(mod, target);`,
    `module.exports = mod.exports;`,
  ].join("\n");
}

/**
 * Replace `@tofu-sdk/core`'s generated multi-platform napi loader with an inlined
 * copy of the single addon present at build time, and generate the shebang and
 * executable bit on the entry. The result is one self-contained file.
 */
function providerBundlePlugin() {
  let bindingDir: string | undefined;
  return {
    name: "tofu-sdk-provider-bundle",
    // Intercept the SDK's `require("../binding/index.js")` (resolved to an
    // absolute path) and redirect it to our virtual replacement module.
    async resolveId(this: any, source: string, importer: string | undefined, options: any) {
      const resolved = await this.resolve(source, importer, { ...options, skipSelf: true });
      if (resolved && /[\\/]binding[\\/]index\.js$/.test(resolved.id)) {
        bindingDir = dirname(resolved.id);
        return NATIVE_MODULE;
      }
      return null;
    },
    load(this: any, id: string) {
      if (id !== NATIVE_MODULE) return null;
      if (!bindingDir) return this.error("tofu-sdk: could not locate @tofu-sdk/core's binding/");
      const addon = readdirSync(bindingDir).find((f) => f.endsWith(".node"));
      if (!addon) {
        return this.error(
          `tofu-sdk: no native addon (*.node) in ${bindingDir} — build @tofu-sdk/core first`,
        );
      }
      return inlineAddonModule(readFileSync(join(bindingDir, addon)));
    },
    // Generate the go-plugin executable's shebang on the entry chunk.
    renderChunk(this: any, code: string, chunk: any) {
      if (chunk.isEntry && !code.startsWith("#!")) return { code: SHEBANG + code, map: null };
      return null;
    },
    // Mark the emitted executable as executable.
    writeBundle(this: any, options: any, bundle: Record<string, any>) {
      for (const file of Object.values(bundle)) {
        if (file.type === "chunk" && file.isEntry) {
          chmodSync(join(options.dir, file.fileName), 0o755);
        }
      }
    },
  };
}

/**
 * Build the tsdown config for a Terraform/OpenTofu provider. Use the result as
 * the default export of your `tsdown.config.ts`.
 */
export function defineProviderBundle(options: ProviderBundleOptions) {
  const { entry, name, outDir = "dist" } = options;
  return {
    entry: [entry],
    format: "cjs" as const,
    platform: "node" as const,
    outDir,
    dts: false,
    // A provider ships as a single self-contained file: bundle every dependency
    // (zod, your cloud SDK, …); only Node built-ins stay external. `onlyBundle:
    // false` silences tsdown's "you bundled a dependency" hint — that's the point.
    deps: {
      alwaysBundle: (id: string) => (id.startsWith("node:") ? undefined : true),
      onlyBundle: false,
    },
    // We name and chmod the executable ourselves; don't let tsdown touch the
    // project's package.json `bin` field.
    bin: false,
    // Name the output exactly as Terraform expects to `exec` it.
    outputOptions: { entryFileNames: `terraform-provider-${name}` },
    plugins: [providerBundlePlugin()],
  };
}
