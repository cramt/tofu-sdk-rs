//! `pg_database` — a PostgreSQL database.
//!
//! Demonstrates an **optional** attribute that is deliberately not drift-refreshed
//! (`owner`): set it to assign ownership, or omit it and let the server choose —
//! an omitted value stays null rather than drifting every plan. The same
//! `DatabaseModel` also backs a **singular `pg_database` data source** (projected
//! via the `exclusive` search key on `name`), exactly like `example-aws`'s
//! bucket; in that projection `owner` is read back as a computed value.

use std::sync::Arc;

use deadpool_postgres::Object;
use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{async_trait, Ctx, DataSource, DataSourceError, Resource, ResourceError};

use crate::config::Pg;
use crate::sql::{ds_err, quote_ident, res_err};

/// A database. `name` forces replacement on rename and is the unique key for the
/// data source lookup. `owner` is optional: provide it to set the owner, or omit
/// it to accept the server's default (see the field doc for why it is not
/// drift-refreshed).
#[derive(Facet)]
#[facet(terraform::resource("pg_database"))]
#[facet(terraform::data_source("pg_database"))]
pub struct DatabaseModel {
    /// Database name. Renaming replaces the database.
    #[facet(terraform::force_new)]
    #[facet(terraform::search_key(exclusive))]
    pub name: String,

    /// Owning role. Optional: set it to assign ownership, or omit it to accept
    /// the server's default (the connecting role). When omitted it stays null in
    /// state — out-of-band owner changes are not reconciled, which keeps an
    /// unset `owner` from producing a perpetual diff. (In the **data source**
    /// projection this same field is read back as a computed value.)
    pub owner: Option<String>,

    /// The database's catalog OID. Computed.
    #[facet(terraform::computed)]
    pub oid: i64,
}

/// Read `(oid, owner)` for a database by name. `None` if it does not exist.
async fn fetch(db: &Object, name: &str) -> Result<Option<(i64, String)>, tokio_postgres::Error> {
    let row = db
        .query_opt(
            "SELECT oid::int8, pg_get_userbyid(datdba) FROM pg_database WHERE datname = $1",
            &[&name],
        )
        .await?;
    Ok(row.map(|r| (r.get(0), r.get(1))))
}

/// The `pg_database` resource handler.
pub struct DatabaseResource {
    pub pg: Arc<Pg>,
}

impl DatabaseResource {
    /// Refresh the computed `oid` from the catalog after a write. `owner` is left
    /// as configured (see the field doc).
    async fn refresh(&self, db: &Object, model: &mut DatabaseModel) -> Result<(), ResourceError> {
        let (oid, _owner) = fetch(db, &model.name)
            .await
            .map_err(|e| res_err("failed to read database", e))?
            .ok_or_else(|| {
                res_err(
                    "database disappeared",
                    format!("{:?} not found after write", model.name),
                )
            })?;
        model.oid = oid;
        Ok(())
    }
}

#[async_trait]
impl Resource for DatabaseResource {
    type Model = DatabaseModel;

    async fn create(
        &self,
        _ctx: &mut Ctx,
        mut planned: DatabaseModel,
    ) -> Result<DatabaseModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let mut sql = format!("CREATE DATABASE {}", quote_ident(&planned.name));
        if let Some(owner) = planned.owner.as_deref().filter(|o| !o.is_empty()) {
            sql.push_str(&format!(" OWNER {}", quote_ident(owner)));
        }
        // CREATE DATABASE cannot run inside a transaction; one statement per
        // batch_execute stays out of an implicit transaction block.
        db.batch_execute(&sql)
            .await
            .map_err(|e| res_err("failed to create database", e))?;
        self.refresh(&db, &mut planned).await?;
        Ok(planned)
    }

    async fn update(
        &self,
        _ctx: &mut Ctx,
        mut planned: DatabaseModel,
        prior: DatabaseModel,
    ) -> Result<DatabaseModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        if planned.owner != prior.owner {
            if let Some(owner) = planned.owner.as_deref().filter(|o| !o.is_empty()) {
                db.batch_execute(&format!(
                    "ALTER DATABASE {} OWNER TO {}",
                    quote_ident(&planned.name),
                    quote_ident(owner)
                ))
                .await
                .map_err(|e| res_err("failed to change database owner", e))?;
            }
        }
        self.refresh(&db, &mut planned).await?;
        Ok(planned)
    }

    async fn read(
        &self,
        _ctx: &mut Ctx,
        mut current: DatabaseModel,
    ) -> Result<Option<DatabaseModel>, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        match fetch(&db, &current.name)
            .await
            .map_err(|e| res_err("failed to read database", e))?
        {
            // Preserve the configured `owner`; only the computed `oid` refreshes.
            Some((oid, _owner)) => {
                current.oid = oid;
                Ok(Some(current))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, _ctx: &mut Ctx, prior: DatabaseModel) -> Result<(), ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        // FORCE (PG13+) terminates any lingering connections so destroy is robust.
        db.batch_execute(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            quote_ident(&prior.name)
        ))
        .await
        .map_err(|e| res_err("failed to drop database", e))?;
        Ok(())
    }

    async fn import(&self, _ctx: &mut Ctx, id: String) -> Result<DatabaseModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let (oid, owner) = fetch(&db, &id)
            .await
            .map_err(|e| res_err("failed to import database", e))?
            .ok_or_else(|| res_err("database not found", format!("no database named {id:?}")))?;
        Ok(DatabaseModel {
            name: id,
            owner: Some(owner),
            oid,
        })
    }
}

/// The singular `pg_database` data source handler (shares `DatabaseModel`).
pub struct DatabaseDataSource {
    pub pg: Arc<Pg>,
}

#[async_trait]
impl DataSource for DatabaseDataSource {
    type Model = DatabaseModel;

    async fn read(
        &self,
        _ctx: &mut Ctx,
        mut query: DatabaseModel,
    ) -> Result<DatabaseModel, DataSourceError> {
        let db = self.pg.get().await.map_err(|e| ds_err("connect", e))?;
        let (oid, owner) = fetch(&db, &query.name)
            .await
            .map_err(|e| ds_err("failed to read database", e))?
            .ok_or_else(|| {
                ds_err(
                    "database not found",
                    format!("no database named {:?}", query.name),
                )
            })?;
        query.oid = oid;
        query.owner = Some(owner);
        Ok(query)
    }
}
