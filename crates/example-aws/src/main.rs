//! A minimal example Terraform/OpenTofu provider built with the SDK.
//!
//! It declares a single resource by reflecting a plain Rust struct and serves
//! its full create/read/update/delete lifecycle over the plugin protocol. The
//! resource has no external backing — it derives its computed attributes
//! (`arn`, `region`) from configuration and the configured provider state —
//! which keeps the example self-contained and deterministic.
//!
//! It also demonstrates **provider configuration**: the provider takes an
//! optional `region`, and `configure` turns that into a shared `AwsClient`
//! (the *meta*) handed to every handler, which stamps the region onto every
//! bucket it creates.
//!
//! It demonstrates **data sources projected from the same model**: the `Bucket`
//! struct carries both `terraform::resource` and `terraform::data_source`
//! markers, and its `search_key` fields drive two read-only lookups — a singular
//! `data "aws_s3_bucket"` keyed by the unique `arn` (one object) and a plural
//! `data "aws_s3_buckets"` keyed by the generic `name` (a `results` list).
//!
//! It demonstrates an **ephemeral resource** `aws_session_token`: a short-lived
//! credential minted for the duration of a run and never written to state,
//! exercising the full `Open`/`Renew`/`Close` lifecycle.
//!
//! Finally it demonstrates a **state store** `inmem`: a provider-defined Terraform
//! backend that keeps each workspace's raw state bytes in memory, exercising
//! `Configure`/`Read`/`Write`/`Lock`/`Unlock`/`GetStates`/`Delete`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{
    async_trait, serve, Action, ActionError, Ctx, DataSource, DataSourceError, DataSourceList,
    Ephemeral, EphemeralError, Function, FunctionError, ListError, ListItem, ListResource,
    Provider, Resource, ResourceError, StateBackend, StateStore, StateStoreError, VariadicFunction,
};

/// Provider-level configuration.
#[derive(Facet)]
#[allow(dead_code)]
struct AwsConfig {
    /// The region buckets are created in. Defaults to `us-east-1` when unset.
    /// An `Option<T>` field is inferred optional — no disposition marker needed.
    region: Option<String>,
}

/// The configured provider state shared by every resource handler. In a real
/// provider this would hold an API client, credentials, an HTTP pool, etc.
struct AwsClient {
    region: String,
}

/// An S3-bucket-like resource — and, via the same model, a data source.
///
/// The `#[facet(terraform::data_source)]` marker and the `search_key` fields
/// project this one model into data sources too: looking a bucket up by its
/// unique `arn` (`exclusive`) yields a single object, while looking up by the
/// generic `name` (`shared`) yields a list. The resource dispositions
/// (`required`/`force_new`/`computed`) and the data source projection are
/// independent — a field can be computed on the resource yet an input on a data
/// source.
#[derive(Facet)]
#[facet(terraform::resource("aws_s3_bucket"))]
#[facet(terraform::data_source("aws_s3_bucket"))]
#[allow(dead_code)]
struct Bucket {
    /// The globally-unique name of the bucket. A generic data source key:
    /// looking up by name may match any number of buckets. A non-`Option` field
    /// is inferred required — no `required` marker needed.
    #[facet(terraform::force_new)]
    #[facet(terraform::search_key(shared))]
    name: String,

    /// The ARN, derived from the name after creation. A unique data source key:
    /// looking up by arn matches at most one bucket.
    #[facet(terraform::computed)]
    #[facet(terraform::search_key(exclusive))]
    arn: String,

    /// The region the bucket lives in, taken from provider configuration.
    #[facet(terraform::computed)]
    region: String,

    /// Which handler last wrote this resource: `"created"` or `"updated"`. It
    /// lets the `tofu test` suite observe whether a change replaced the bucket
    /// (the create path runs again) or updated it in place.
    #[facet(terraform::computed)]
    last_action: String,

    /// Free-form tags.
    tags: Option<HashMap<String, String>>,
}

/// The handler for `aws_s3_bucket`, holding the configured client.
struct BucketResource {
    client: Arc<AwsClient>,
}

impl BucketResource {
    /// Fill the computed attributes from the (known) name and configured region,
    /// recording which lifecycle handler ran in `last_action`.
    fn computed(&self, mut bucket: Bucket, action: &str) -> Bucket {
        bucket.arn = format!("arn:aws:s3:::{}", bucket.name);
        bucket.region = self.client.region.clone();
        bucket.last_action = action.to_string();
        bucket
    }
}

#[async_trait]
impl Resource for BucketResource {
    type Model = Bucket;

    async fn create(&self, _ctx: &mut Ctx, planned: Bucket) -> Result<Bucket, ResourceError> {
        Ok(self.computed(planned, "created"))
    }

    async fn update(
        &self,
        _ctx: &mut Ctx,
        planned: Bucket,
        _prior: Bucket,
    ) -> Result<Bucket, ResourceError> {
        Ok(self.computed(planned, "updated"))
    }
}

/// A resource with a **write-only** input, demonstrating that a secret supplied
/// at apply time is used by the handler but never persisted to state.
#[derive(Facet)]
#[facet(terraform::resource("aws_locker"))]
#[allow(dead_code)]
struct Locker {
    // The name is also the resource's stable identity (used for import-by-identity
    // and identity tracking). It is known from config, so the identity is stable
    // across plan and apply.
    #[facet(terraform::force_new)]
    #[facet(terraform::identity)]
    name: String,

    /// The secret to store. Write-only: it reaches the handler through the
    /// apply-time config, but the runtime nulls it out of every persisted state.
    #[facet(terraform::write_only)]
    secret: Option<String>,

    /// Whether `create` observed a non-empty secret. A computed witness that the
    /// write-only value genuinely reached the handler — even though `secret`
    /// itself is null in state.
    #[facet(terraform::computed)]
    has_secret: bool,

    /// A superseded alias for `name`, kept only to demonstrate the `deprecated`
    /// schema flag (the engine surfaces it as a deprecation warning).
    #[facet(terraform::deprecated("use `name` instead"))]
    legacy_name: Option<String>,
}

/// The handler for `aws_locker`. It records *whether* a secret was supplied (so a
/// test can prove the handler saw it) without ever echoing the secret back.
struct LockerResource;

#[async_trait]
impl Resource for LockerResource {
    type Model = Locker;

    async fn create(&self, _ctx: &mut Ctx, mut planned: Locker) -> Result<Locker, ResourceError> {
        planned.has_secret = planned.secret.as_deref().is_some_and(|s| !s.is_empty());
        Ok(planned)
    }

    async fn update(
        &self,
        _ctx: &mut Ctx,
        mut planned: Locker,
        _prior: Locker,
    ) -> Result<Locker, ResourceError> {
        planned.has_secret = planned.secret.as_deref().is_some_and(|s| !s.is_empty());
        Ok(planned)
    }
}

/// Query inputs for the `aws_locker` list resource: an optional name-prefix
/// filter. A list resource's config block is a *separate* type from the resource
/// model — it holds query parameters, not resource attributes.
#[derive(Facet)]
#[allow(dead_code)]
struct LockerFilter {
    /// Only return lockers whose name starts with this prefix (all when unset).
    name_prefix: Option<String>,
}

/// The `aws_locker` list resource: enumerate existing lockers, optionally filtered
/// by name prefix. It shares the `Locker` model with the managed resource, so each
/// result projects into the resource's `name` identity by construction.
struct LockerList;

#[async_trait]
impl ListResource for LockerList {
    type Model = Locker;
    type Config = LockerFilter;

    async fn list(
        &self,
        _ctx: &mut Ctx,
        config: LockerFilter,
    ) -> Result<Vec<ListItem<Locker>>, ListError> {
        // A real provider would enumerate the remote API; synthesize a fixed set
        // so the suite stays deterministic and self-contained.
        let prefix = config.name_prefix.unwrap_or_default();
        Ok(["alpha", "beta", "gamma"]
            .into_iter()
            .filter(|name| name.starts_with(&prefix))
            .map(|name| {
                ListItem::new(
                    format!("locker {name}"),
                    Locker {
                        name: name.to_string(),
                        secret: None,
                        has_secret: false,
                        legacy_name: None,
                    },
                )
            })
            .collect())
    }
}

/// Config for the `aws_publish` action: a message to publish to a named topic.
/// An action's config is a plain block — no state, no computed results.
#[derive(Facet)]
#[allow(dead_code)]
struct PublishConfig {
    /// The topic to publish to.
    topic: String,
    /// The message body.
    message: String,
}

/// The `aws_publish` action: a provider-defined imperative operation. It has no
/// state — `invoke` performs a side effect (here, a synthesized "publish") and
/// streams progress back to the host.
struct Publish;

#[async_trait]
impl Action for Publish {
    type Config = PublishConfig;

    async fn validate(
        &self,
        _ctx: &mut Ctx,
        config: PublishConfig,
    ) -> Vec<terraform_runtime::Diag> {
        if config.topic.is_empty() {
            return vec![terraform_runtime::Diag::error(
                "topic must not be empty",
                "set a non-empty `topic` for the publish action",
            )];
        }
        Vec::new()
    }

    async fn invoke(&self, ctx: &mut Ctx, config: PublishConfig) -> Result<(), ActionError> {
        // A real action would call the remote API; emit progress so the host shows
        // the work, then report success.
        ctx.progress(format!("publishing to {}", config.topic));
        ctx.progress(format!("published: {}", config.message));
        Ok(())
    }
}

/// The strip-prefix used to derive a bucket name from its ARN, and vice versa.
const ARN_PREFIX: &str = "arn:aws:s3:::";

/// The singular `aws_s3_bucket` data source: look a bucket up by its unique
/// `arn` (the `exclusive` search key) and resolve to one object. Shares the
/// `Bucket` model and the configured `AwsClient` with the resource.
struct BucketByArn {
    client: Arc<AwsClient>,
}

#[async_trait]
impl DataSource for BucketByArn {
    type Model = Bucket;

    async fn read(&self, _ctx: &mut Ctx, mut query: Bucket) -> Result<Bucket, DataSourceError> {
        // The query arrives with `arn` set (the exclusive key); recover the rest.
        query.name = query
            .arn
            .strip_prefix(ARN_PREFIX)
            .unwrap_or(&query.arn)
            .to_string();
        query.region = self.client.region.clone();
        query.last_action = "read".to_string();
        Ok(query)
    }
}

/// The plural `aws_s3_buckets` data source: look buckets up by the generic
/// `name` (the `shared` search key) and resolve to a `results` list. The example
/// has no backing store, so it synthesizes a couple of matches to demonstrate
/// the list shape.
struct BucketsByName {
    client: Arc<AwsClient>,
}

#[async_trait]
impl DataSourceList for BucketsByName {
    type Model = Bucket;

    async fn list(&self, _ctx: &mut Ctx, query: Bucket) -> Result<Vec<Bucket>, DataSourceError> {
        let region = self.client.region.clone();
        let matches = ["", "-staging"]
            .iter()
            .map(|suffix| {
                let name = format!("{}{suffix}", query.name);
                Bucket {
                    arn: format!("{ARN_PREFIX}{name}"),
                    region: region.clone(),
                    last_action: "read".to_string(),
                    tags: None,
                    name,
                }
            })
            .collect();
        Ok(matches)
    }
}

/// The positional parameters of the `arn_for` function: a single bucket name.
#[derive(Facet)]
#[allow(dead_code)]
struct ArnForArgs {
    /// The bucket name to build an ARN for.
    name: String,
}

/// A pure provider-defined function `arn_for(name)` that builds a bucket ARN —
/// callable from HCL as `provider::aws::arn_for("my-bucket")`. Functions take no
/// provider configuration or state.
struct ArnFor;

#[async_trait]
impl Function for ArnFor {
    type Params = ArnForArgs;
    type Output = String;

    async fn call(&self, params: ArnForArgs) -> Result<String, FunctionError> {
        Ok(format!("{ARN_PREFIX}{}", params.name))
    }
}

/// The fixed leading parameters of `join`: the separator.
#[derive(Facet)]
#[allow(dead_code)]
struct JoinArgs {
    /// The separator placed between the variadic parts.
    separator: String,
}

/// A **variadic** function `join(separator, parts…)` — a fixed leading parameter
/// plus zero or more trailing arguments. Called from HCL as
/// `provider::aws::join("-", "a", "b", "c")` → `"a-b-c"`.
struct Join;

#[async_trait]
impl VariadicFunction for Join {
    type Params = JoinArgs;
    type VarArg = String;
    type Output = String;

    async fn call(&self, params: JoinArgs, parts: Vec<String>) -> Result<String, FunctionError> {
        Ok(parts.join(&params.separator))
    }
}

/// An **ephemeral resource** `aws_session_token`: a short-lived credential minted
/// for the duration of a single Terraform run and *never written to state*.
///
/// It demonstrates the full `Open` → `Renew` → `Close` lifecycle: `open` mints a
/// token and asks Terraform to renew before its TTL, `renew` re-arms the window,
/// and `close` revokes it. The role is stashed in private state on `open` because
/// `renew`/`close` receive only those bytes — not the config or the result.
#[derive(Facet)]
#[facet(terraform::ephemeral("aws_session_token"))]
#[allow(dead_code)]
struct SessionToken {
    /// The role to mint a session token for. A required config input.
    role: String,

    /// The minted, short-lived token. Computed by `open`, never persisted to
    /// state, and marked sensitive so it is redacted in logs.
    #[facet(terraform::computed)]
    #[facet(terraform::sensitive)]
    token: String,
}

/// The handler for the `aws_session_token` ephemeral resource, holding the
/// configured client (so the minted token reflects the provider's region).
struct SessionTokenEphemeral {
    client: Arc<AwsClient>,
}

/// Pretend lease TTL; we ask Terraform to renew at half of it.
const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);

#[async_trait]
impl Ephemeral for SessionTokenEphemeral {
    type Model = SessionToken;

    async fn open(
        &self,
        ctx: &mut Ctx,
        mut config: SessionToken,
    ) -> Result<SessionToken, EphemeralError> {
        // A real provider would call STS; synthesize a token to stay
        // deterministic and self-contained.
        config.token = format!("tok-{}-{}", config.role, self.client.region);
        // `close`/`renew` get only the private bytes — stash what they need.
        ctx.set_private(config.role.clone().into_bytes());
        ctx.renew_after(TOKEN_TTL / 2);
        Ok(config)
    }

    async fn renew(&self, ctx: &mut Ctx) -> Result<(), EphemeralError> {
        // The handle is the role stashed on open; re-arm the renewal window.
        let _role = String::from_utf8_lossy(ctx.private());
        ctx.renew_after(TOKEN_TTL / 2);
        Ok(())
    }

    async fn close(&self, ctx: &mut Ctx) -> Result<(), EphemeralError> {
        // Revoke the token for the stashed role (a no-op in this example).
        let _role = String::from_utf8_lossy(ctx.private());
        Ok(())
    }
}

/// Configuration for the in-memory state store: an optional key prefix applied to
/// every state id, so two store instances can share the process without clashing.
#[derive(Facet)]
#[allow(dead_code)]
struct InMemConfig {
    /// Namespace prepended to every state id. Optional; defaults to empty.
    prefix: Option<String>,
}

/// The shared, process-global storage behind the `inmem` state store: state bytes
/// and held locks, both keyed by the (prefixed) state id. Cloning shares the maps.
#[derive(Clone, Default)]
struct InMemStore {
    states: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    locks: Arc<Mutex<HashMap<String, String>>>,
}

/// A connected [`InMemStore`] backend bound to a configured prefix.
struct InMemBackend {
    prefix: String,
    store: InMemStore,
}

impl InMemBackend {
    /// The storage key for a state id (the configured prefix plus the id).
    fn key(&self, state_id: &str) -> String {
        format!("{}{state_id}", self.prefix)
    }
}

/// Monotonic source of lock identifiers (no `rand`/clock needed in the example).
static LOCK_SEQ: AtomicU64 = AtomicU64::new(1);

#[async_trait]
impl StateStore for InMemStore {
    type Config = InMemConfig;
    type Backend = InMemBackend;

    async fn configure(&self, config: InMemConfig) -> Result<InMemBackend, StateStoreError> {
        Ok(InMemBackend {
            prefix: config.prefix.unwrap_or_default(),
            store: self.clone(),
        })
    }
}

#[async_trait]
impl StateBackend for InMemBackend {
    async fn read_state(&self, state_id: String) -> Result<Vec<u8>, StateStoreError> {
        let states = self.store.states.lock().unwrap();
        Ok(states
            .get(&self.key(&state_id))
            .cloned()
            .unwrap_or_default())
    }

    async fn write_state(&self, state_id: String, data: Vec<u8>) -> Result<(), StateStoreError> {
        let mut states = self.store.states.lock().unwrap();
        states.insert(self.key(&state_id), data);
        Ok(())
    }

    async fn lock(&self, state_id: String, operation: String) -> Result<String, StateStoreError> {
        let mut locks = self.store.locks.lock().unwrap();
        let key = self.key(&state_id);
        if let Some(held) = locks.get(&key) {
            return Err(StateStoreError::new("state is already locked")
                .with_detail(format!("`{state_id}` is held by lock {held}")));
        }
        let lock_id = format!("{operation}-{}", LOCK_SEQ.fetch_add(1, Ordering::Relaxed));
        locks.insert(key, lock_id.clone());
        Ok(lock_id)
    }

    async fn unlock(&self, state_id: String, lock_id: String) -> Result<(), StateStoreError> {
        let mut locks = self.store.locks.lock().unwrap();
        let key = self.key(&state_id);
        match locks.get(&key) {
            Some(held) if *held == lock_id => {
                locks.remove(&key);
                Ok(())
            }
            Some(held) => Err(StateStoreError::new("lock id mismatch")
                .with_detail(format!("`{state_id}` is held by {held}, not {lock_id}"))),
            None => Ok(()),
        }
    }

    async fn states(&self) -> Result<Vec<String>, StateStoreError> {
        let states = self.store.states.lock().unwrap();
        Ok(states
            .keys()
            .filter_map(|k| k.strip_prefix(&self.prefix).map(str::to_string))
            .collect())
    }

    async fn delete_state(&self, state_id: String) -> Result<(), StateStoreError> {
        let mut states = self.store.states.lock().unwrap();
        states.remove(&self.key(&state_id));
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .provider_config::<AwsConfig>()
        .configure(|cfg: AwsConfig| async move {
            Arc::new(AwsClient {
                region: cfg.region.unwrap_or_else(|| "us-east-1".to_string()),
            })
        })
        .resource_with(|client: Arc<AwsClient>| BucketResource { client })
        .resource(LockerResource)
        .list_resource(LockerList)
        .data_source_with(|client: Arc<AwsClient>| BucketByArn { client })
        .data_source_list_with(|client: Arc<AwsClient>| BucketsByName { client })
        .ephemeral_with(|client: Arc<AwsClient>| SessionTokenEphemeral { client })
        .function("arn_for", ArnFor)
        .function_variadic("join", Join)
        .state_store("inmem", InMemStore::default())
        .action("aws_publish", Publish)
        .build()
        .expect("provider definition is valid");

    if let Err(err) = serve(provider).await {
        eprintln!("example-aws: failed to serve: {err}");
        std::process::exit(1);
    }
}
