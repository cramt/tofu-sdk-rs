//! `pg_role` — a PostgreSQL role (a "user" when it can log in).
//!
//! Demonstrates: a write-only secret (`password`), boolean attributes with
//! schema defaults, in-place `ALTER` updates, drift detection via `pg_roles`,
//! import by name, and config validation. Also defines the plural **`pg_roles`
//! data source** (a dedicated, password-free model) to show a list lookup.

use std::sync::Arc;

use deadpool_postgres::Object;
use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{
    async_trait, Ctx, DataSourceError, DataSourceList, Diag, Resource, ResourceError,
};

use crate::config::Pg;
use crate::sql::{ds_err, quote_ident, quote_literal, res_err};

/// A role / login user. `name` is the primary key and forces replacement on
/// rename. The boolean attributes default in the schema so they are always known
/// (no perpetual diff against the catalog). `password` is write-only — it reaches
/// the handler at apply time but is never persisted to state.
#[derive(Facet)]
#[facet(terraform::resource("pg_role"))]
pub struct RoleModel {
    /// Role name. Renaming replaces the role.
    #[facet(terraform::force_new)]
    pub name: String,

    /// Whether the role may log in (i.e. is a "user"). Defaults to `true`.
    #[facet(terraform::default("true"))]
    pub login: bool,

    /// Whether the role is a superuser. Defaults to `false`.
    #[facet(terraform::default("false"))]
    pub superuser: bool,

    /// Whether the role may create databases. Defaults to `false`.
    #[facet(terraform::default("false"))]
    pub create_db: bool,

    /// Whether the role may create other roles. Defaults to `false`.
    #[facet(terraform::default("false"))]
    pub create_role: bool,

    /// The role's password. Write-only: used by `create`/`update` but nulled out
    /// of every persisted state.
    #[facet(terraform::write_only)]
    pub password: Option<String>,

    /// The role's catalog OID. Computed.
    #[facet(terraform::computed)]
    pub oid: i64,
}

/// Render the `WITH …` attribute clause shared by `CREATE`/`ALTER ROLE`.
/// `password` is interpolated as a quoted literal because DDL cannot bind it.
fn role_options(model: &RoleModel) -> String {
    let mut opts = vec![
        if model.login { "LOGIN" } else { "NOLOGIN" }.to_string(),
        if model.superuser { "SUPERUSER" } else { "NOSUPERUSER" }.to_string(),
        if model.create_db { "CREATEDB" } else { "NOCREATEDB" }.to_string(),
        if model.create_role { "CREATEROLE" } else { "NOCREATEROLE" }.to_string(),
    ];
    if let Some(password) = model.password.as_deref().filter(|p| !p.is_empty()) {
        opts.push(format!("PASSWORD {}", quote_literal(password)));
    }
    opts.join(" ")
}

/// Fetch a role's catalog OID by name, erroring if it has vanished.
async fn fetch_oid(db: &Object, name: &str) -> Result<i64, ResourceError> {
    let row = db
        .query_opt("SELECT oid::int8 FROM pg_roles WHERE rolname = $1", &[&name])
        .await
        .map_err(|e| res_err("failed to read role oid", e))?
        .ok_or_else(|| res_err("role disappeared", format!("role {name:?} not found after write")))?;
    Ok(row.get(0))
}

/// The `pg_role` handler.
pub struct RoleResource {
    pub pg: Arc<Pg>,
}

#[async_trait]
impl Resource for RoleResource {
    type Model = RoleModel;

    async fn validate(&self, _ctx: &mut Ctx, config: RoleModel) -> Vec<Diag> {
        let mut diags = Vec::new();
        if config.name.trim().is_empty() {
            diags.push(Diag::error("invalid role name", "name must not be empty").at("name"));
        } else if config.name.starts_with("pg_") {
            diags.push(
                Diag::error(
                    "reserved role name",
                    "role names beginning with `pg_` are reserved by PostgreSQL",
                )
                .at("name"),
            );
        }
        diags
    }

    async fn create(&self, _ctx: &mut Ctx, mut planned: RoleModel) -> Result<RoleModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        db.batch_execute(&format!(
            "CREATE ROLE {} WITH {}",
            quote_ident(&planned.name),
            role_options(&planned)
        ))
        .await
        .map_err(|e| res_err("failed to create role", e))?;
        planned.oid = fetch_oid(&db, &planned.name).await?;
        Ok(planned)
    }

    async fn update(
        &self,
        _ctx: &mut Ctx,
        mut planned: RoleModel,
        _prior: RoleModel,
    ) -> Result<RoleModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        db.batch_execute(&format!(
            "ALTER ROLE {} WITH {}",
            quote_ident(&planned.name),
            role_options(&planned)
        ))
        .await
        .map_err(|e| res_err("failed to alter role", e))?;
        planned.oid = fetch_oid(&db, &planned.name).await?;
        Ok(planned)
    }

    async fn read(
        &self,
        _ctx: &mut Ctx,
        mut current: RoleModel,
    ) -> Result<Option<RoleModel>, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let row = db
            .query_opt(
                "SELECT oid::int8, rolcanlogin, rolsuper, rolcreatedb, rolcreaterole \
                 FROM pg_roles WHERE rolname = $1",
                &[&current.name],
            )
            .await
            .map_err(|e| res_err("failed to read role", e))?;
        let Some(row) = row else { return Ok(None) };
        current.oid = row.get(0);
        current.login = row.get(1);
        current.superuser = row.get(2);
        current.create_db = row.get(3);
        current.create_role = row.get(4);
        Ok(Some(current))
    }

    async fn delete(&self, _ctx: &mut Ctx, prior: RoleModel) -> Result<(), ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        db.batch_execute(&format!("DROP ROLE IF EXISTS {}", quote_ident(&prior.name)))
            .await
            .map_err(|e| res_err("failed to drop role", e))?;
        Ok(())
    }

    async fn import(&self, _ctx: &mut Ctx, id: String) -> Result<RoleModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let row = db
            .query_opt(
                "SELECT oid::int8, rolcanlogin, rolsuper, rolcreatedb, rolcreaterole \
                 FROM pg_roles WHERE rolname = $1",
                &[&id],
            )
            .await
            .map_err(|e| res_err("failed to import role", e))?
            .ok_or_else(|| res_err("role not found", format!("no role named {id:?}")))?;
        Ok(RoleModel {
            name: id,
            login: row.get(1),
            superuser: row.get(2),
            create_db: row.get(3),
            create_role: row.get(4),
            password: None,
            oid: row.get(0),
        })
    }
}

// --- pg_roles: a plural list data source -----------------------------------
//
// A dedicated, secret-free model (so the lookup never surfaces a `password`
// field). The `shared` search key makes `name` an optional filter and the
// projection wraps the matches in a computed `results = list(object(...))`.

/// One element of the `pg_roles` list, and the query shape.
#[derive(Facet)]
#[facet(terraform::data_source("pg_role"))]
pub struct RoleQuery {
    /// Optional exact-match filter on role name. Omit to list every role.
    #[facet(terraform::search_key(shared))]
    pub name: String,

    /// The role's catalog OID. Computed.
    #[facet(terraform::computed)]
    pub oid: i64,

    /// Whether the role can log in. Computed.
    #[facet(terraform::computed)]
    pub can_login: bool,

    /// Whether the role is a superuser. Computed.
    #[facet(terraform::computed)]
    pub superuser: bool,
}

/// The `pg_roles` list handler.
pub struct RolesDataSource {
    pub pg: Arc<Pg>,
}

#[async_trait]
impl DataSourceList for RolesDataSource {
    type Model = RoleQuery;

    async fn list(&self, _ctx: &mut Ctx, query: RoleQuery) -> Result<Vec<RoleQuery>, DataSourceError> {
        let db = self.pg.get().await.map_err(|e| ds_err("connect", e))?;
        // An unset search key arrives as the zero value (empty string) → list all.
        let rows = if query.name.is_empty() {
            db.query(
                "SELECT rolname, oid::int8, rolcanlogin, rolsuper FROM pg_roles ORDER BY rolname",
                &[],
            )
            .await
        } else {
            db.query(
                "SELECT rolname, oid::int8, rolcanlogin, rolsuper FROM pg_roles \
                 WHERE rolname = $1 ORDER BY rolname",
                &[&query.name],
            )
            .await
        }
        .map_err(|e| ds_err("failed to list roles", e))?;

        Ok(rows
            .into_iter()
            .map(|row| RoleQuery {
                name: row.get(0),
                oid: row.get(1),
                can_login: row.get(2),
                superuser: row.get(3),
            })
            .collect())
    }
}
