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
 * base64-inlined into the bundle and `dlopen`ed at runtime. On Linux it loads
 * from an anonymous `/dev/shm` (RAM) file so it never touches disk; elsewhere it
 * falls back to a per-hash file in `$TMPDIR`, written once. All of this —
 * bundling the author's deps, embedding the platform binary, generating the
 * shebang and the executable bit — lives in here, not in the author's config.
 *
 * Caveat: the addon is `dlopen`ed from `/dev/shm` (Linux) or `$TMPDIR`; a
 * `noexec` mount on both will prevent it from loading (rare — the loader tries
 * `/dev/shm` then `$TMPDIR`, so set `TMPDIR` to an exec-capable path if you hit
 * it).
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
 * Generate the CommonJS module that inlines the addon `bytes` (as base64) and, at
 * runtime, materializes them so they can be `dlopen`ed.
 *
 * On Linux it loads from an **anonymous** `/dev/shm` (tmpfs) file via
 * `/proc/self/fd` — the addon lives in RAM, never touches disk, and vanishes with
 * the process. Anywhere else (or if that path fails — e.g. `/dev/shm` is absent or
 * `noexec`) it falls back to a per-hash file in `$TMPDIR`, written once.
 */
function inlineAddonModule(bytes: Buffer): string {
  const b64 = bytes.toString("base64");
  const hash = createHash("sha256").update(bytes).digest("hex").slice(0, 16);
  return [
    `const fs = require("node:fs");`,
    `const buf = Buffer.from(${JSON.stringify(b64)}, "base64");`,
    `const mod = { exports: {} };`,
    `function writeAll(fd) { let off = 0; while (off < buf.length) off += fs.writeSync(fd, buf, off, buf.length - off); }`,
    // Linux: an anonymous tmpfs file (RAM-backed), dlopen'd via /proc/self/fd.
    `function loadViaShm() {`,
    `  const path = "/dev/shm/tofu-sdk-addon-" + process.pid + "-" + process.hrtime.bigint();`,
    `  const fd = fs.openSync(path, "w+");`,
    `  try {`,
    `    fs.unlinkSync(path);`, // now anonymous: no on-disk name, freed when the fd closes
    `    writeAll(fd);`,
    `    process.dlopen(mod, "/proc/self/fd/" + fd);`, // fd stays open for the process's life
    `  } catch (e) { fs.closeSync(fd); throw e; }`,
    `}`,
    // Portable fallback: a per-hash file in $TMPDIR, materialized once.
    `function loadViaTemp() {`,
    `  const { join } = require("node:path");`,
    `  const { tmpdir } = require("node:os");`,
    `  const target = join(tmpdir(), "tofu-sdk-addon-${hash}.node");`,
    `  if (!fs.existsSync(target)) {`,
    `    const scratch = join(fs.mkdtempSync(join(tmpdir(), "tofu-sdk-addon-")), "addon.node");`,
    `    fs.writeFileSync(scratch, buf);`,
    `    try { fs.renameSync(scratch, target); } catch { /* lost the race; another process wrote it */ }`,
    `  }`,
    `  process.dlopen(mod, target);`,
    `}`,
    `if (process.platform === "linux") { try { loadViaShm(); } catch { loadViaTemp(); } } else { loadViaTemp(); }`,
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
