// Bundle the example provider into a single self-contained executable:
//
//   npx tsdown --config examples/tsdown.config.mjs   # run from packages/tofu-sdk
//
// Output: examples/dist/terraform-provider-cloudflare — one file (the `cloudflare`
// SDK, zod, and the native addon all inlined), with a generated shebang and the
// executable bit set. Nothing else to ship.
//
// Authors normally write this as `tsdown.config.ts`:
//
//   import { defineProviderBundle } from "@tofu-sdk/core/tsdown";
//   export default defineProviderBundle({ entry: "src/provider.ts", name: "cloudflare" });
//
// It's `.mjs` here only because this package is `"type": "commonjs"`, where Node
// (< 24.11) can't load a `.ts` tsdown config without `--config-loader tsx`.
import { defineProviderBundle } from "@tofu-sdk/core/tsdown";

export default defineProviderBundle({
  entry: "cloudflare-provider.ts",
  name: "cloudflare",
});
