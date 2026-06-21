//! `pg_schema` — a schema (namespace) inside the connected database.

use std::sync::Arc;

use deadpool_postgres::Object;
use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{async_trait, Ctx, Resource, ResourceError};

use crate::config::Pg;
use crate::sql::{quote_ident, res_err};

/// A schema. `name` forces replacement on rename; `owner` is optional-computed
/// (set it, or read back whoever the server assigned — the connecting role).
#[derive(Facet)]
#[facet(terraform::resource("pg_schema"))]
pub struct SchemaModel {
    /// Schema name. Renaming replaces the schema.
    #[facet(terraform::force_new)]
    pub name: String,

    /// Owning role. Optional: set it to assign ownership, or omit it to accept
    /// the server's default (the connecting role). When omitted it stays null in
    /// state, so an unset `owner` never produces a perpetual diff.
    pub owner: Option<String>,

    /// The schema's catalog OID. Computed.
    #[facet(terraform::computed)]
    pub oid: i64,
}

/// Read `(oid, owner)` for a schema by name. `None` if it does not exist.
async fn fetch(db: &Object, name: &str) -> Result<Option<(i64, String)>, tokio_postgres::Error> {
    let row = db
        .query_opt(
            "SELECT oid::int8, pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname = $1",
            &[&name],
        )
        .await?;
    Ok(row.map(|r| (r.get(0), r.get(1))))
}

/// The `pg_schema` resource handler.
pub struct SchemaResource {
    pub pg: Arc<Pg>,
}

impl SchemaResource {
    /// Refresh the computed `oid`; `owner` is left as configured (see field doc).
    async fn refresh(&self, db: &Object, model: &mut SchemaModel) -> Result<(), ResourceError> {
        let (oid, _owner) = fetch(db, &model.name)
            .await
            .map_err(|e| res_err("failed to read schema", e))?
            .ok_or_else(|| {
                res_err(
                    "schema disappeared",
                    format!("{:?} not found after write", model.name),
                )
            })?;
        model.oid = oid;
        Ok(())
    }
}

#[async_trait]
impl Resource for SchemaResource {
    type Model = SchemaModel;

    async fn create(
        &self,
        _ctx: &mut Ctx,
        mut planned: SchemaModel,
    ) -> Result<SchemaModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let mut sql = format!("CREATE SCHEMA {}", quote_ident(&planned.name));
        if let Some(owner) = planned.owner.as_deref().filter(|o| !o.is_empty()) {
            sql.push_str(&format!(" AUTHORIZATION {}", quote_ident(owner)));
        }
        db.batch_execute(&sql)
            .await
            .map_err(|e| res_err("failed to create schema", e))?;
        self.refresh(&db, &mut planned).await?;
        Ok(planned)
    }

    async fn update(
        &self,
        _ctx: &mut Ctx,
        mut planned: SchemaModel,
        prior: SchemaModel,
    ) -> Result<SchemaModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        if planned.owner != prior.owner {
            if let Some(owner) = planned.owner.as_deref().filter(|o| !o.is_empty()) {
                db.batch_execute(&format!(
                    "ALTER SCHEMA {} OWNER TO {}",
                    quote_ident(&planned.name),
                    quote_ident(owner)
                ))
                .await
                .map_err(|e| res_err("failed to change schema owner", e))?;
            }
        }
        self.refresh(&db, &mut planned).await?;
        Ok(planned)
    }

    async fn read(
        &self,
        _ctx: &mut Ctx,
        mut current: SchemaModel,
    ) -> Result<Option<SchemaModel>, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        match fetch(&db, &current.name)
            .await
            .map_err(|e| res_err("failed to read schema", e))?
        {
            // Preserve the configured `owner`; only the computed `oid` refreshes.
            Some((oid, _owner)) => {
                current.oid = oid;
                Ok(Some(current))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, _ctx: &mut Ctx, prior: SchemaModel) -> Result<(), ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        // RESTRICT (the default): destroy fails loudly if objects remain, rather
        // than silently cascading. Terraform destroys contained resources first.
        db.batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {}",
            quote_ident(&prior.name)
        ))
        .await
        .map_err(|e| res_err("failed to drop schema", e))?;
        Ok(())
    }

    async fn import(&self, _ctx: &mut Ctx, id: String) -> Result<SchemaModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let (oid, owner) = fetch(&db, &id)
            .await
            .map_err(|e| res_err("failed to import schema", e))?
            .ok_or_else(|| res_err("schema not found", format!("no schema named {id:?}")))?;
        Ok(SchemaModel {
            name: id,
            owner: Some(owner),
            oid,
        })
    }
}
