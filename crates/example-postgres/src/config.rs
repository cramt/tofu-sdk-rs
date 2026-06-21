//! Provider configuration and the shared connection pool (`Pg`, the *meta*).
//!
//! Every connection field is optional and falls back to the standard `libpq`
//! environment variable (`PGHOST`, `PGPORT`, …) and then a default — the same
//! precedence the official Postgres tools use, so the provider works out of the
//! box in a configured shell. [`configure`] builds a `deadpool` pool and pings
//! it once so a bad endpoint fails fast with a clear diagnostic instead of
//! surfacing on the first resource.

use std::sync::Arc;

use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, PoolError, RecyclingMethod};
use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::Diag;
use tokio_postgres::NoTls;

const DEFAULT_HOST: &str = "localhost";
const DEFAULT_PORT: u16 = 5432;
const DEFAULT_USER: &str = "postgres";
const DEFAULT_DATABASE: &str = "postgres";
/// Pool ceiling. Terraform applies resources with bounded parallelism (10 by
/// default), so a small pool comfortably covers it.
const POOL_MAX_SIZE: usize = 10;

/// Provider-level configuration. All fields are optional; each resolves from the
/// HCL value, else the `PG*` environment variable, else a built-in default.
#[derive(Facet, Default)]
pub struct PgProviderConfig {
    /// Server host. Falls back to `PGHOST`, then `localhost`.
    pub host: Option<String>,
    /// Server port. Falls back to `PGPORT`, then `5432`.
    pub port: Option<i64>,
    /// Role to authenticate as. Falls back to `PGUSER`, then `postgres`.
    pub username: Option<String>,
    /// Password. Falls back to `PGPASSWORD`. Sensitive: redacted in logs and
    /// never echoed back into state.
    #[facet(terraform::sensitive)]
    pub password: Option<String>,
    /// Database to connect to (the one schemas/extensions/tables are created in;
    /// roles and databases are cluster-global regardless). Falls back to
    /// `PGDATABASE`, then `postgres`.
    pub database: Option<String>,
    /// TLS mode. This template builds `tokio-postgres` with `NoTls`, so only
    /// `disable` is accepted; see the README for wiring up `rustls`. Falls back
    /// to `PGSSLMODE`, then `disable`.
    pub sslmode: Option<String>,
}

/// The configured provider state handed to every handler: a pooled connection to
/// PostgreSQL.
pub struct Pg {
    pool: Pool,
}

impl Pg {
    /// Check out a pooled connection. The returned [`Object`] dereferences to a
    /// [`tokio_postgres::Client`] and returns to the pool when dropped.
    pub async fn get(&self) -> Result<Object, PoolError> {
        self.pool.get().await
    }
}

/// Resolve a string setting: explicit HCL value, else the environment variable,
/// else the default.
fn resolve(explicit: Option<String>, env_key: &str, default: &str) -> String {
    explicit
        .or_else(|| std::env::var(env_key).ok())
        .unwrap_or_else(|| default.to_string())
}

/// Build the connection pool from provider config, validating connectivity once.
/// Returns a [`Diag`] (surfaced by Terraform as a provider-config error) if the
/// settings are invalid or the server is unreachable.
pub async fn configure(cfg: PgProviderConfig) -> Result<Arc<Pg>, Diag> {
    let host = resolve(cfg.host, "PGHOST", DEFAULT_HOST);
    let user = resolve(cfg.username, "PGUSER", DEFAULT_USER);
    let database = resolve(cfg.database, "PGDATABASE", DEFAULT_DATABASE);
    let sslmode = resolve(cfg.sslmode, "PGSSLMODE", "disable");
    let password = cfg.password.or_else(|| std::env::var("PGPASSWORD").ok());

    let port: u16 = cfg
        .port
        .and_then(|p| u16::try_from(p).ok())
        .or_else(|| std::env::var("PGPORT").ok().and_then(|s| s.parse().ok()))
        .unwrap_or(DEFAULT_PORT);

    if sslmode != "disable" {
        return Err(Diag::error(
            "unsupported sslmode",
            format!(
                "this provider is built with tokio-postgres `NoTls`; only \
                 sslmode=disable is supported (got {sslmode:?}). See the crate \
                 README, \"Enabling TLS\", to wire up rustls."
            ),
        )
        .at(["sslmode"]));
    }

    let mut pg_config = tokio_postgres::Config::new();
    pg_config
        .host(&host)
        .port(port)
        .user(&user)
        .dbname(&database);
    if let Some(password) = &password {
        pg_config.password(password);
    }

    let manager = Manager::from_config(
        pg_config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool = Pool::builder(manager)
        .max_size(POOL_MAX_SIZE)
        .build()
        .map_err(|e| Diag::error("failed to build connection pool", e.to_string()))?;

    // Fail fast: prove we can actually reach the server before any resource runs.
    let client = pool.get().await.map_err(|e| {
        Diag::error(
            "cannot connect to PostgreSQL",
            format!("{user}@{host}:{port}/{database}: {e}"),
        )
    })?;
    client
        .simple_query("SELECT 1")
        .await
        .map_err(|e| Diag::error("PostgreSQL connectivity check failed", e.to_string()))?;
    drop(client);

    Ok(Arc::new(Pg { pool }))
}
