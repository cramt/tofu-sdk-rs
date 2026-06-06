# tofu-sdk-rs test harness

An iteration-sequence test harness that exercises the SDK against a **real**
`tofu`/`terraform` engine and asserts the provider's observable side effects.

It pairs:

- **`example-fs`** (`../crates/example-fs`) — a deliberately side-effecting
  example provider. Each `fs_file` resource writes its attributes to
  `<output_dir>/<name>.json` on create/update and deletes that file on destroy.
- **this harness** — Vitest. For each *configuration*, it stands up one
  `dev_overrides` workspace and applies an ordered sequence of *iterations* into
  it, asserting the set of JSON files after each step.

## The shared-state trick

You asked how iterations share state without an S3 backend. They don't need one:
the **local backend keeps `terraform.tfstate` in the working directory**. The
harness uses one persistent config dir per configuration and, between
iterations, swaps the resource `.tf` files in place (keeping a harness-owned
provider block) and re-runs `apply`. Because the state file stays put, iteration
2 sees iteration 1's resources — so updates, force-new replacements, and deletes
all happen naturally across the sequence.

## Layout

```
harness/
  configs/
    <config-name>/
      1/                 # iteration 1
        *.tf             # resource definitions (no provider block — see below)
        expected/        # the exact set of JSON files expected after this apply
          <name>.json
      2/                 # iteration 2 (applied on top of 1's state)
        ...
  src/harness.ts         # workspace + apply + discovery helpers
  harness.test.ts        # describe.each(configs) -> it.each(iterations)
```

- Iteration folders sort **numerically** (`1` < `2` < `10`).
- `*.tf` files hold only resources/data/variables. The harness injects the
  `terraform { required_providers … }` block and the `provider "fs"` block
  (with a per-configuration temp `output_dir`) as `_harness.tf`, which persists
  across iterations.
- `expected/` is compared structurally (parsed JSON, key order ignored) against
  the output dir. A file present in the output but absent from `expected/` (or
  vice versa) fails the iteration — so deletes are asserted by *omission*.

## Running

Inside the Nix dev shell (provides `tofu`, `node`, `pnpm`, and `PROTOC` for the
cargo build):

```bash
nix develop --command bash -c 'cd harness && pnpm install && pnpm test'
```

`globalSetup` builds `example-fs` once via `cargo build -p example-fs`.

## Adding a configuration

1. Create `configs/<name>/1/`, `configs/<name>/2/`, … with `*.tf` resources.
2. For each iteration, add `expected/<name>.json` for every file that should
   exist in the output dir *after that apply* (cumulative — list everything
   still present, not just what changed).
3. Run `pnpm test`. The new configuration is discovered automatically.

The recorded `action` field (`"created"` / `"updated"`) reflects which handler
ran, so you can assert create-vs-update and replacement behavior directly. It
lives only in the written file (not the resource schema), which keeps it free of
Terraform's "computed values may not change during an in-place update"
consistency rule.
