//! A Terraform/OpenTofu provider for "the nix stuff".
//!
//! Two primitives, mirroring how a Nix-deployed host is rolled out:
//!
//! - a **`nix_build` data source** that realises a flake attribute — e.g.
//!   `nixosConfigurations.toolbox.config.system.build.toplevel` — by shelling out
//!   to `nix build`, and exposes the resulting store path (and derivation path).
//!   It runs at plan/refresh time, so "the thing" is built before anything is
//!   deployed.
//!
//! - a **`nix_deploy` resource** that takes a built store path and a target host,
//!   copies the closure to it (`nix copy --to ssh://…`) and activates it
//!   (`switch-to-configuration`, setting the system profile for `switch`/`boot`).
//!   Re-applies re-deploy in place; changing the `target_host` forces a new
//!   deployment.
//!
//! This is the first slice of replacing the `nixos-anywhere` modules that
//! infra-dev-toolbox currently uses: it handles copy-closure + activate of an
//! already-installed host. Partition/install of a fresh machine is a later step.
//!
//! Use it from another workspace via a `dev_overrides` block pointing
//! `example/nix` at this binary (built as `terraform-provider-nix`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use facet::Facet;
use terraform_codec::{from_value, to_value};
use terraform_ir::{AttributeSchema, Block};
use terraform_provider::terraform;
use terraform_runtime::{
    async_trait, serve, Ctx, Diag, Diagnostics, DynDataSource, Provider, Resource, ResourceError,
};
use terraform_value::{Type, Value};
use tokio::process::Command;

/// Experimental-features flag passed to every `nix` invocation, so the provider
/// works even when the host's `nix.conf` hasn't enabled flakes globally.
const NIX_FEATURES: &str = "nix-command flakes";

/// Run a program to completion, returning its stdout on success or a
/// human-readable error (including stderr) on failure.
async fn run(
    program: &str,
    args: &[String],
    extra_env: &[(&str, String)],
) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("could not run `{program}`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The first non-empty, trimmed line of some command output.
fn first_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// nix_build — build a flake attribute, expose its store path.
// ---------------------------------------------------------------------------

/// The `nix_build` data source model. `flake`/`attr` are inputs, `special_args`
/// an optional input, `out_path`/`drv_path` the computed outputs.
///
/// It uses the **dynamic seam** (a hand-built [`Block`] + [`DynDataSource`])
/// rather than reflection, because the typed data-source projection only allows
/// *required* inputs (search keys) — and `special_args` must be optional. The
/// codec ([`from_value`]/[`to_value`]) still marshals this plain struct, so the
/// handler stays typed.
#[derive(Facet)]
struct NixBuild {
    /// Flake reference: `"."`, an absolute path, or e.g. `github:owner/repo`.
    flake: String,
    /// Attribute path within the flake to build, e.g.
    /// `nixosConfigurations.toolbox.config.system.build.toplevel`.
    attr: String,
    /// Optional `specialArgs` to inject into the NixOS evaluation, as a JSON
    /// object string — use `jsonencode({ … })`. Replicates nixos-anywhere's
    /// `extendModules { specialArgs = …; }`; only valid when `attr` selects a
    /// `nixosConfigurations.<name>.config.…` output.
    special_args: Option<String>,
    /// The realised output store path (`/nix/store/…`).
    out_path: String,
    /// The derivation path (`/nix/store/….drv`) that produced `out_path`.
    /// Best-effort: empty if it can't be resolved.
    drv_path: String,
}

/// The hand-built schema for `nix_build` (see [`NixBuild`] for why it isn't
/// reflected).
fn nix_build_block() -> Block {
    let attr = |name: &str, required: bool, computed: bool| AttributeSchema {
        name: name.to_string(),
        ty: Type::String,
        description: None,
        required,
        optional: !required && !computed,
        computed,
        sensitive: false,
        write_only: false,
        force_new: false,
        deprecated: None,
        default: None,
    };
    Block {
        attributes: vec![
            attr("flake", true, false),
            attr("attr", true, false),
            attr("special_args", false, false),
            attr("out_path", false, true),
            attr("drv_path", false, true),
        ],
        nested_blocks: Vec::new(),
    }
}

/// Pull a string field out of a (dynamic JSON) object value.
fn json_field(value: &facet_value::Value, key: &str) -> Option<String> {
    Some(
        value
            .as_object()?
            .get(key)?
            .as_string()?
            .as_str()
            .to_string(),
    )
}

/// Split a NixOS build attribute into the config path and the build attribute,
/// the same way nixos-anywhere's nix-build.sh does: the config path is
/// everything before the **last** `.config.`, the build attribute everything
/// from the **first** (prefixed back with `config.`). For the usual single
/// `.config.` the two agree.
///
/// `nixosConfigurations.toolbox.config.system.build.toplevel`
/// → (`nixosConfigurations.toolbox`, `config.system.build.toplevel`).
fn split_nixos_attr(attr: &str) -> Result<(&str, String), String> {
    const MARKER: &str = ".config.";
    let first = attr.find(MARKER).ok_or_else(|| {
        format!(
            "`special_args` needs an `attr` containing `{MARKER}` \
             (e.g. nixosConfigurations.<name>.config.system.build.toplevel); got `{attr}`"
        )
    })?;
    let last = attr.rfind(MARKER).expect("find implies rfind");
    let config_path = &attr[..last];
    let config_attribute = format!("config.{}", &attr[first + MARKER.len()..]);
    Ok((config_path, config_attribute))
}

/// The trailing `nix build` installable arguments for `(flake, attr,
/// special_args)`. With no special args it's simply `<flake>#<attr>`. With
/// special args it replicates nixos-anywhere's `extendModules { specialArgs = …
/// }` trick: prefetch the flake to a pure `path:` URL, then build the toplevel
/// from an `--expr` so the injected `specialArgs` reach the evaluation. The same
/// trailing args drive both the build and the (best-effort) derivation lookup.
async fn installable_args(
    flake: &str,
    attr: &str,
    special_args: Option<&str>,
) -> Result<Vec<String>, String> {
    let special = special_args
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "{}");
    let Some(special) = special else {
        return Ok(vec![format!("{flake}#{attr}")]);
    };

    let (config_path, config_attribute) = split_nixos_attr(attr)?;

    // Pin the flake into the store as a pure `path:` URL so `builtins.getFlake`
    // is reproducible.
    let prefetch = run(
        "nix",
        &[
            "flake".into(),
            "prefetch".into(),
            flake.to_string(),
            "--json".into(),
            "--extra-experimental-features".into(),
            NIX_FEATURES.into(),
        ],
        &[],
    )
    .await?;
    let parsed: facet_value::Value = facet_json::from_str(&prefetch)
        .map_err(|e| format!("could not parse `nix flake prefetch` output: {e}"))?;
    let store_path =
        json_field(&parsed, "storePath").ok_or("`nix flake prefetch` output had no storePath")?;
    let nar_hash = json_field(&parsed, "hash").ok_or("`nix flake prefetch` output had no hash")?;
    let flake_url = format!("path:{store_path}?narHash={nar_hash}");

    let expr = format!(
        "(builtins.getFlake ''{flake_url}'').{config_path}.extendModules \
         {{ specialArgs = builtins.fromJSON ''{special}''; }}"
    );
    Ok(vec!["--expr".into(), expr, config_attribute])
}

/// Handler for `nix_build`. Stateless — every read shells out to `nix`.
struct NixBuilder;

#[async_trait]
impl DynDataSource for NixBuilder {
    async fn read(&self, config: Value) -> Result<Value, Diagnostics> {
        let mut model: NixBuild = from_value(&config)
            .map_err(|e| vec![Diag::error("decode nix_build config", e.to_string())])?;
        let target = format!("{}#{}", model.flake, model.attr);

        let trailing = installable_args(&model.flake, &model.attr, model.special_args.as_deref())
            .await
            .map_err(|e| vec![Diag::error("nix build setup failed", e)])?;

        let mut build_args = vec![
            "build".into(),
            "--no-link".into(),
            "--print-out-paths".into(),
            "--extra-experimental-features".into(),
            NIX_FEATURES.into(),
        ];
        build_args.extend(trailing.iter().cloned());
        let stdout = run("nix", &build_args, &[])
            .await
            .map_err(|e| vec![Diag::error("nix build failed", e)])?;
        model.out_path = first_line(&stdout).ok_or_else(|| {
            vec![Diag::error(
                "nix build produced no output path",
                format!("building `{target}` printed nothing"),
            )]
        })?;

        // Derivation path is informational; never fail the read over it.
        let mut drv_args = vec![
            "path-info".into(),
            "--derivation".into(),
            "--extra-experimental-features".into(),
            NIX_FEATURES.into(),
        ];
        drv_args.extend(trailing);
        model.drv_path = run("nix", &drv_args, &[])
            .await
            .ok()
            .and_then(|s| first_line(&s))
            .unwrap_or_default();

        to_value(&model).map_err(|e| vec![Diag::error("encode nix_build state", e.to_string())])
    }

    async fn validate(&self, _config: Value) -> Diagnostics {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// nix_deploy — copy a closure to a host and activate it.
// ---------------------------------------------------------------------------

/// The `nix_deploy` resource: deploy a built store path to a target host.
#[derive(Facet)]
#[facet(terraform::resource("nix_deploy"))]
struct NixDeploy {
    /// The store path to deploy — typically `data.nix_build.<name>.out_path`.
    /// Changing it re-deploys in place.
    store_path: String,

    /// The target host (`hostname` or an IP). Changing it forces a fresh
    /// deployment rather than an in-place switch.
    #[facet(terraform::force_new)]
    target_host: String,

    /// SSH user to connect as. Defaults to `root`.
    ssh_user: Option<String>,

    /// PEM/OpenSSH private key used for the connection. When unset, the agent /
    /// default keys are used. Never written to state in cleartext beyond the
    /// usual state-file caveat — mark it sensitive.
    #[facet(terraform::sensitive)]
    ssh_private_key: Option<String>,

    /// `switch-to-configuration` action: `switch` (default), `boot`, `test`, or
    /// `dry-activate`. `switch`/`boot` also set the system profile.
    action: Option<String>,

    /// The store path that was activated on the host (echoes `store_path` on a
    /// successful deploy). Lets downstream config depend on the deploy completing.
    #[facet(terraform::computed)]
    activated_path: String,
}

/// Valid `switch-to-configuration` actions.
const ACTIONS: [&str; 4] = ["switch", "boot", "test", "dry-activate"];

/// Monotonic suffix so concurrent deploys don't share a temp key path.
static KEY_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp file holding a private key, removed when dropped.
struct KeyFile(PathBuf);

impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl KeyFile {
    /// Write `contents` to a fresh `0600` temp file.
    fn write(contents: &str) -> Result<Self, String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let seq = KEY_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("tofu-nix-deploy-{}-{seq}.key", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("could not create temp key file: {e}"))?;
        file.write_all(contents.as_bytes())
            .map_err(|e| format!("could not write temp key file: {e}"))?;
        // ssh rejects keys without a trailing newline.
        if !contents.ends_with('\n') {
            file.write_all(b"\n")
                .map_err(|e| format!("could not write temp key file: {e}"))?;
        }
        Ok(KeyFile(path))
    }
}

/// The remote command that activates `path` with `action`. `switch`/`boot` set
/// the system profile first so the activation survives a reboot; `test` and
/// `dry-activate` deliberately leave the profile untouched.
fn activation_command(path: &str, action: &str) -> String {
    let switch = format!("{path}/bin/switch-to-configuration {action}");
    match action {
        "switch" | "boot" => {
            format!("nix-env --profile /nix/var/nix/profiles/system --set {path} && {switch}")
        }
        _ => switch,
    }
}

/// Copy the closure to the host and activate it.
async fn deploy(model: &NixDeploy) -> Result<(), String> {
    let user = model.ssh_user.as_deref().unwrap_or("root");
    let action = model.action.as_deref().unwrap_or("switch");
    let dest = format!("{user}@{}", model.target_host);

    // A temp key (kept alive for the whole deploy) plus the ssh options shared by
    // `nix copy` (via NIX_SSHOPTS) and the activation `ssh`.
    let key = match model.ssh_private_key.as_deref() {
        Some(k) if !k.is_empty() => Some(KeyFile::write(k)?),
        _ => None,
    };
    let mut ssh_opts: Vec<String> = vec![
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "BatchMode=yes".into(),
    ];
    if let Some(key) = &key {
        ssh_opts.push("-i".into());
        ssh_opts.push(key.0.display().to_string());
    }

    // 1. Copy the closure to the target.
    let copy_args = vec![
        "copy".into(),
        "--to".into(),
        format!("ssh://{dest}"),
        model.store_path.clone(),
        "--extra-experimental-features".into(),
        NIX_FEATURES.into(),
    ];
    run("nix", &copy_args, &[("NIX_SSHOPTS", ssh_opts.join(" "))]).await?;

    // 2. Activate on the target.
    let mut ssh_args = ssh_opts;
    ssh_args.push(dest);
    ssh_args.push(activation_command(&model.store_path, action));
    run("ssh", &ssh_args, &[]).await?;

    Ok(())
}

/// Handler for `nix_deploy`. Create and update both run the same deploy.
struct NixDeployer;

impl NixDeployer {
    async fn apply(&self, mut model: NixDeploy) -> Result<NixDeploy, ResourceError> {
        deploy(&model)
            .await
            .map_err(|e| ResourceError::new("nix deploy failed").with_detail(e))?;
        model.activated_path = model.store_path.clone();
        Ok(model)
    }
}

#[async_trait]
impl Resource for NixDeployer {
    type Model = NixDeploy;

    async fn validate(&self, _ctx: &mut Ctx, config: NixDeploy) -> Vec<Diag> {
        match config.action.as_deref() {
            Some(action) if !ACTIONS.contains(&action) => vec![Diag::error(
                "invalid `action`",
                format!("`{action}` is not one of {}", ACTIONS.join(", ")),
            )
            .at(["action"])],
            _ => Vec::new(),
        }
    }

    async fn create(&self, _ctx: &mut Ctx, planned: NixDeploy) -> Result<NixDeploy, ResourceError> {
        self.apply(planned).await
    }

    async fn update(
        &self,
        _ctx: &mut Ctx,
        planned: NixDeploy,
        _prior: NixDeploy,
    ) -> Result<NixDeploy, ResourceError> {
        self.apply(planned).await
    }

    // delete defaults to a no-op: a NixOS host can't be "unswitched", so we just
    // drop the resource from state. read defaults to passthrough (no drift check).
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .dyn_data_source("nix_build", nix_build_block(), Arc::new(NixBuilder))
        .resource(NixDeployer)
        .build()
        .expect("provider definition is valid");

    if let Err(err) = serve(provider).await {
        eprintln!("example-nix: failed to serve: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_trims_and_skips_blanks() {
        assert_eq!(
            first_line("\n  \n /nix/store/abc \n/nix/store/def\n").as_deref(),
            Some("/nix/store/abc")
        );
        assert_eq!(first_line("   \n\n").as_deref(), None);
        assert_eq!(first_line("").as_deref(), None);
    }

    #[test]
    fn switch_and_boot_set_the_system_profile() {
        let cmd = activation_command("/nix/store/sys", "switch");
        assert!(cmd.contains("nix-env --profile /nix/var/nix/profiles/system --set /nix/store/sys"));
        assert!(cmd.ends_with("/nix/store/sys/bin/switch-to-configuration switch"));

        assert!(activation_command("/nix/store/sys", "boot")
            .contains("--profile /nix/var/nix/profiles/system"));
    }

    #[test]
    fn test_and_dry_activate_leave_the_profile_untouched() {
        for action in ["test", "dry-activate"] {
            let cmd = activation_command("/nix/store/sys", action);
            assert!(
                !cmd.contains("nix-env"),
                "{action} must not set the profile"
            );
            assert_eq!(
                cmd,
                format!("/nix/store/sys/bin/switch-to-configuration {action}")
            );
        }
    }

    #[test]
    fn split_nixos_attr_separates_config_path_and_build_attribute() {
        let (path, build) =
            split_nixos_attr("nixosConfigurations.toolbox.config.system.build.toplevel").unwrap();
        assert_eq!(path, "nixosConfigurations.toolbox");
        assert_eq!(build, "config.system.build.toplevel");
    }

    #[test]
    fn split_nixos_attr_requires_a_config_segment() {
        let err = split_nixos_attr("packages.x86_64-linux.hello").unwrap_err();
        assert!(
            err.contains(".config."),
            "error should name the marker: {err}"
        );
    }

    #[tokio::test]
    async fn installable_args_without_special_args_is_just_flake_hash_attr() {
        assert_eq!(
            installable_args(".", "packages.x86_64-linux.hello", None)
                .await
                .unwrap(),
            vec![".#packages.x86_64-linux.hello"]
        );
        // An empty object is treated as "no special args".
        assert_eq!(
            installable_args(".", "x", Some("{}")).await.unwrap(),
            vec![".#x"]
        );
        assert_eq!(
            installable_args(".", "x", Some("   ")).await.unwrap(),
            vec![".#x"]
        );
    }

    #[tokio::test]
    async fn installable_args_with_special_args_needs_a_config_attr() {
        let err = installable_args(".", "packages.x86_64-linux.hello", Some(r#"{"a":1}"#))
            .await
            .unwrap_err();
        assert!(
            err.contains(".config."),
            "error should name the marker: {err}"
        );
    }

    #[test]
    fn key_file_is_0600_with_trailing_newline_and_cleans_up() {
        use std::os::unix::fs::PermissionsExt;

        let path;
        {
            let key = KeyFile::write("PRIVATE-KEY-BODY").expect("write temp key");
            path = key.0.clone();
            let meta = std::fs::metadata(&path).expect("temp key exists");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            let contents = std::fs::read_to_string(&path).expect("read temp key");
            assert_eq!(contents, "PRIVATE-KEY-BODY\n");
        }
        // Dropped: the file is removed.
        assert!(!path.exists(), "temp key should be cleaned up on drop");
    }
}
