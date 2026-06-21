//! `pg_extension` — an installed extension (e.g. `pgcrypto`, `citext`).
//!
//! Demonstrates a settable `version` that can be upgraded in place
//! (`ALTER EXTENSION … UPDATE`) alongside a computed `installed_version` read
//! back from the catalog.

use std::sync::Arc;

use deadpool_postgres::Object;
use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{async_trait, Ctx, Resource, ResourceError};

use crate::config::Pg;
use crate::sql::{quote_ident, quote_literal, res_err};

/// An extension. `name` and `schema` force replacement; `version` is the desired
/// version (upgradable in place) and `installed_version` is what is actually
/// installed.
#[derive(Facet)]
#[facet(terraform::resource("pg_extension"))]
pub struct ExtensionModel {
    /// Extension name (must be available to the server, e.g. from `contrib`).
    #[facet(terraform::force_new)]
    pub name: String,

    /// Schema to install the extension into. Changing it replaces the extension.
    #[facet(terraform::force_new)]
    pub schema: Option<String>,

    /// Desired version. Omit to install the default; set to upgrade in place.
    pub version: Option<String>,

    /// The version actually installed, read from `pg_extension`. Computed.
    #[facet(terraform::computed)]
    pub installed_version: String,

    /// The extension's catalog OID. Computed.
    #[facet(terraform::computed)]
    pub oid: i64,
}

/// Read `(oid, installed_version)` for an extension by name. `None` if absent.
async fn fetch(db: &Object, name: &str) -> Result<Option<(i64, String)>, tokio_postgres::Error> {
    let row = db
        .query_opt(
            "SELECT oid::int8, extversion FROM pg_extension WHERE extname = $1",
            &[&name],
        )
        .await?;
    Ok(row.map(|r| (r.get(0), r.get(1))))
}

/// The `pg_extension` resource handler.
pub struct ExtensionResource {
    pub pg: Arc<Pg>,
}

impl ExtensionResource {
    async fn refresh(&self, db: &Object, model: &mut ExtensionModel) -> Result<(), ResourceError> {
        let (oid, installed) = fetch(db, &model.name)
            .await
            .map_err(|e| res_err("failed to read extension", e))?
            .ok_or_else(|| {
                res_err(
                    "extension disappeared",
                    format!("{:?} not found after write", model.name),
                )
            })?;
        model.oid = oid;
        model.installed_version = installed;
        Ok(())
    }
}

#[async_trait]
impl Resource for ExtensionResource {
    type Model = ExtensionModel;

    async fn create(
        &self,
        _ctx: &mut Ctx,
        mut planned: ExtensionModel,
    ) -> Result<ExtensionModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let mut sql = format!(
            "CREATE EXTENSION IF NOT EXISTS {}",
            quote_ident(&planned.name)
        );
        if let Some(schema) = planned.schema.as_deref().filter(|s| !s.is_empty()) {
            sql.push_str(&format!(" WITH SCHEMA {}", quote_ident(schema)));
        }
        if let Some(version) = planned.version.as_deref().filter(|v| !v.is_empty()) {
            sql.push_str(&format!(" VERSION {}", quote_literal(version)));
        }
        db.batch_execute(&sql)
            .await
            .map_err(|e| res_err("failed to create extension", e))?;
        self.refresh(&db, &mut planned).await?;
        Ok(planned)
    }

    async fn update(
        &self,
        _ctx: &mut Ctx,
        mut planned: ExtensionModel,
        prior: ExtensionModel,
    ) -> Result<ExtensionModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        if planned.version != prior.version {
            let mut sql = format!("ALTER EXTENSION {} UPDATE", quote_ident(&planned.name));
            if let Some(version) = planned.version.as_deref().filter(|v| !v.is_empty()) {
                sql.push_str(&format!(" TO {}", quote_literal(version)));
            }
            db.batch_execute(&sql)
                .await
                .map_err(|e| res_err("failed to update extension", e))?;
        }
        self.refresh(&db, &mut planned).await?;
        Ok(planned)
    }

    async fn read(
        &self,
        _ctx: &mut Ctx,
        mut current: ExtensionModel,
    ) -> Result<Option<ExtensionModel>, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        match fetch(&db, &current.name)
            .await
            .map_err(|e| res_err("failed to read extension", e))?
        {
            Some((oid, installed)) => {
                current.oid = oid;
                current.installed_version = installed;
                Ok(Some(current))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, _ctx: &mut Ctx, prior: ExtensionModel) -> Result<(), ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        db.batch_execute(&format!(
            "DROP EXTENSION IF EXISTS {}",
            quote_ident(&prior.name)
        ))
        .await
        .map_err(|e| res_err("failed to drop extension", e))?;
        Ok(())
    }

    async fn import(&self, _ctx: &mut Ctx, id: String) -> Result<ExtensionModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let (oid, installed) = fetch(&db, &id)
            .await
            .map_err(|e| res_err("failed to import extension", e))?
            .ok_or_else(|| res_err("extension not found", format!("no extension named {id:?}")))?;
        Ok(ExtensionModel {
            name: id,
            schema: None,
            version: None,
            installed_version: installed,
            oid,
        })
    }
}
