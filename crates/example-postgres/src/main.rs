//! A production-shaped PostgreSQL provider, meant to be copied as a template.
//!
//! It manages real server objects over a pooled connection: **roles**
//! (`pg_role`), **databases** (`pg_database`), **schemas** (`pg_schema`),
//! **extensions** (`pg_extension`) and **tables** (`pg_table`), plus two data
//! sources — a singular `pg_database` (projected from the resource model) and a
//! plural `pg_roles` list. Each resource implements the full lifecycle with
//! drift detection (`read`), import, and config validation; `pg_table` adds a
//! `modify_plan` replacement rule over nested `column` blocks.
//!
//! Connection settings come from the provider block, falling back to the
//! standard `PG*` environment variables (see [`config`]). Run it against a local
//! server with the `dev_overrides` recipe in the crate README, or let the
//! contract suite (`tests/`) spin Postgres up in Docker and drive `tofu test`.

mod config;
mod database;
mod extension;
mod role;
mod schema;
mod sql;
mod table;

use std::sync::Arc;

use terraform_runtime::{serve, Provider};

use config::{Pg, PgProviderConfig};
use database::{DatabaseDataSource, DatabaseResource};
use extension::ExtensionResource;
use role::{RoleResource, RolesDataSource};
use schema::SchemaResource;
use table::TableResource;

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .provider_config::<PgProviderConfig>()
        .configure(config::configure)
        // Resources.
        .resource_with(|pg: Arc<Pg>| RoleResource { pg })
        .resource_with(|pg: Arc<Pg>| DatabaseResource { pg })
        .resource_with(|pg: Arc<Pg>| SchemaResource { pg })
        .resource_with(|pg: Arc<Pg>| ExtensionResource { pg })
        .resource_with(|pg: Arc<Pg>| TableResource { pg })
        // Data sources: a singular projection and a plural list.
        .data_source_with(|pg: Arc<Pg>| DatabaseDataSource { pg })
        .data_source_list_with(|pg: Arc<Pg>| RolesDataSource { pg })
        .build()
        .expect("provider definition is valid");

    if let Err(err) = serve(provider).await {
        eprintln!("terraform-provider-postgres: failed to serve: {err}");
        std::process::exit(1);
    }
}
