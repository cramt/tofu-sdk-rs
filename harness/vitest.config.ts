import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // Build the provider once, before any test file runs.
    globalSetup: ["./vitest.global-setup.ts"],
    // Real apply/destroy cycles against tofu are slow; be generous.
    testTimeout: 120_000,
    hookTimeout: 180_000,
    // Each configuration drives a real engine in its own workspace; keep files
    // serial so output stays readable and machine load predictable.
    fileParallelism: false,
  },
});
