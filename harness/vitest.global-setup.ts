import { buildProvider } from "./src/harness";

// Vitest global setup: compile the example-fs provider binary once up front so
// every workspace can dev_override the same freshly-built artifact.
export default async function setup(): Promise<void> {
  await buildProvider();
}
