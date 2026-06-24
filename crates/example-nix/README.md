# example-nix — a Terraform/OpenTofu provider for the Nix workflow

Built with `tofu-sdk-rs`. Two primitives that mirror how a Nix-deployed host is
rolled out:

| Kind | Name | What it does |
|------|------|--------------|
| data source | `nix_build` | realises a flake attribute with `nix build`, exposes the store path |
| resource | `nix_deploy` | copies a closure to a host (`nix copy`) and activates it (`switch-to-configuration`) |

This is the first slice of replacing the `nixos-anywhere` modules that
`infra-dev-toolbox` uses: it covers **copy-closure + activate** of an
already-installed host. Partition/install of a fresh machine is a later step.

## `nix_build` (data source)

```hcl
data "nix_build" "toolbox" {
  flake = "."
  attr  = "nixosConfigurations.toolbox.config.system.build.toplevel"

  # Optional: inject NixOS specialArgs (secrets, ssh keys, datasources, …),
  # exactly like nixos-anywhere's `special_args`. Pass a JSON object string.
  special_args = jsonencode({
    name           = "toolbox"
    root_public_key = tls_private_key.deploy_key.public_key_openssh
    # …
  })
}
```

| Field | | Description |
|-------|--|-------------|
| `flake` | required | flake ref: `"."`, an absolute path, `github:owner/repo`, `nixpkgs`, … |
| `attr` | required | attribute path within the flake to build |
| `special_args` | optional | JSON object (use `jsonencode({…})`) injected as NixOS `specialArgs` |
| `out_path` | computed | realised store path (`/nix/store/…`) |
| `drv_path` | computed | derivation that produced it (best-effort) |

Without `special_args` it shells out to `nix build "<flake>#<attr>" --no-link
--print-out-paths` at plan/refresh time. With `special_args` it replicates
nixos-anywhere's `nix-build.sh`: prefetch the flake to a pure `path:` URL, then
build the toplevel from
`(builtins.getFlake "<url>").<config>.extendModules { specialArgs = fromJSON "…"; }`.
This requires `attr` to select a `nixosConfigurations.<name>.config.…` output,
and the flake to have a `flake.lock` (getFlake runs in pure mode). Either way
"the thing" is built before anything is deployed.

## `nix_deploy` (resource)

```hcl
resource "nix_deploy" "toolbox" {
  store_path      = data.nix_build.toolbox.out_path
  target_host     = scaleway_instance_ip.public_ip.address
  ssh_user        = "root"
  ssh_private_key = tls_private_key.deploy_key.private_key_openssh
  action          = "switch" # switch (default) | boot | test | dry-activate
}
```

| Field | | Description |
|-------|--|-------------|
| `store_path` | required | the closure to deploy; changing it re-deploys in place |
| `target_host` | required, force-new | host/IP; changing it forces a fresh deployment |
| `ssh_user` | optional | defaults to `root` |
| `ssh_private_key` | optional, sensitive | connection key; falls back to the agent/default keys when unset |
| `action` | optional | `switch-to-configuration` action |
| `activated_path` | computed | echoes `store_path` once the deploy succeeds |

On create/update it runs, against the target:

1. `nix copy --to ssh://<user>@<host> <store_path>`
2. for `switch`/`boot`: `nix-env --profile /nix/var/nix/profiles/system --set <store_path>`
3. `<store_path>/bin/switch-to-configuration <action>`

`delete` is a no-op (a NixOS host can't be "unswitched"); the resource just
leaves state. `nix` and `ssh` must be on the provider process's `PATH`.

## Running it locally

```bash
cargo build -p example-nix
DIR=$(mktemp -d); ln -s "$PWD/target/debug/example-nix" "$DIR/terraform-provider-nix"
cat > "$DIR/tofurc" <<EOF
provider_installation {
  dev_overrides { "example/nix" = "$DIR" }
  direct {}
}
EOF
cat > "$DIR/main.tf" <<'EOF'
terraform {
  required_providers {
    nix = { source = "example/nix" }
  }
}
data "nix_build" "hello" {
  flake = "nixpkgs"
  attr  = "hello"
}
output "out_path" { value = data.nix_build.hello.out_path }
EOF
(cd "$DIR" && TF_CLI_CONFIG_FILE="$DIR/tofurc" tofu plan)
```

## Wiring into infra-dev-toolbox

Point a `dev_overrides` at the built binary (as above), then replace the
`module "deploy"` (`nixos-anywhere//terraform/all-in-one`) call with the
`nix_build` data source + `nix_deploy` resource once the host is installed. The
`special_args` map the all-in-one module passes maps directly onto
`nix_build`'s `special_args` (wrap the existing map in `jsonencode(...)`).

Still on the all-in-one module and not yet covered here: partition/install of a
fresh machine (disko + the kexec installer) — that's the remaining step before
`nix_deploy` can stand entirely on its own.
