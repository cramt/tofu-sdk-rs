//! `pg_table` — a table defined by repeatable `column { … }` blocks.
//!
//! Demonstrates two SDK features together: **nested blocks** (`column` is a
//! `Vec<Column>` marked `#[facet(terraform::block)]`) and a **`modify_plan`
//! rule** that forces replacement when the column set changes (this template
//! recreates the table rather than emitting `ALTER TABLE`s — documented, and
//! intentionally conservative about data).

use std::sync::Arc;

use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{async_trait, Ctx, Diag, PlanModifications, Resource, ResourceError};

use crate::config::Pg;
use crate::sql::{quote_ident, res_err};

/// A single `column { … }` block.
#[derive(Facet, Clone, PartialEq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// SQL type expression, e.g. `text`, `integer`, `varchar(255)`. Interpolated
    /// into DDL verbatim (it is not an identifier), so it is validated against a
    /// conservative character allowlist.
    pub data_type: String,
    /// Whether the column accepts NULL. Optional; `true` when omitted.
    pub nullable: Option<bool>,
}

/// A table. `name` and `schema` force replacement; the `column` blocks force
/// replacement via [`modify_plan`](Resource::modify_plan) when they change.
#[derive(Facet)]
#[facet(terraform::resource("pg_table"))]
pub struct TableModel {
    /// Table name. Renaming replaces the table.
    #[facet(terraform::force_new)]
    pub name: String,

    /// Schema the table lives in. Optional; `public` when omitted. Changing it
    /// replaces the table.
    #[facet(terraform::force_new)]
    pub schema: Option<String>,

    /// The table's columns, in order. At least one is required.
    #[facet(terraform::block)]
    pub column: Vec<Column>,

    /// The table's catalog OID. Computed.
    #[facet(terraform::computed)]
    pub oid: i64,
}

/// A column type is interpolated into DDL, so restrict it to characters that
/// cannot break out of the type position (no quotes, semicolons, etc.).
fn is_safe_type(data_type: &str) -> bool {
    !data_type.trim().is_empty()
        && data_type
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " ,()[]_".contains(c))
}

/// The effective schema for a table — the configured value, else `public`
/// (the handler-side default for the optional `schema` attribute).
fn schema_of(model: &TableModel) -> &str {
    model.schema.as_deref().unwrap_or("public")
}

/// Render `"schema"."name"`.
fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Fetch a table's OID by (schema, name). `None` if it does not exist.
async fn fetch_oid(
    db: &deadpool_postgres::Object,
    schema: &str,
    name: &str,
) -> Result<Option<i64>, tokio_postgres::Error> {
    let row = db
        .query_opt(
            "SELECT c.oid::int8 FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = $1 AND n.nspname = $2 AND c.relkind = 'r'",
            &[&name, &schema],
        )
        .await?;
    Ok(row.map(|r| r.get(0)))
}

/// The `pg_table` resource handler.
pub struct TableResource {
    pub pg: Arc<Pg>,
}

#[async_trait]
impl Resource for TableResource {
    type Model = TableModel;

    async fn validate(&self, _ctx: &mut Ctx, config: TableModel) -> Vec<Diag> {
        let mut diags = Vec::new();
        // `column` arrives empty when unset or not yet known; only flag a
        // genuinely empty literal list, which the schema also can't express.
        if config.column.is_empty() {
            diags.push(Diag::error(
                "table needs columns",
                "a pg_table must declare at least one `column` block",
            ));
        }
        for col in &config.column {
            if col.name.trim().is_empty() {
                diags.push(Diag::error(
                    "invalid column",
                    "column name must not be empty",
                ));
            }
            if !is_safe_type(&col.data_type) {
                diags.push(Diag::error(
                    "invalid column type",
                    format!(
                        "column {:?} has an unsupported type expression {:?} \
                         (allowed: letters, digits, spaces and ` ,()[]_`)",
                        col.name, col.data_type
                    ),
                ));
            }
        }
        diags
    }

    async fn modify_plan(
        &self,
        _ctx: &mut Ctx,
        prior: Option<TableModel>,
        proposed: TableModel,
    ) -> Result<PlanModifications, ResourceError> {
        // Recreate (rather than ALTER) when the columns change.
        if let Some(prior) = prior {
            if prior.column != proposed.column {
                return Ok(PlanModifications::new().require_replace("column"));
            }
        }
        Ok(PlanModifications::new())
    }

    async fn create(
        &self,
        _ctx: &mut Ctx,
        mut planned: TableModel,
    ) -> Result<TableModel, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let columns = planned
            .column
            .iter()
            .map(|col| {
                let null = if col.nullable.unwrap_or(true) {
                    ""
                } else {
                    " NOT NULL"
                };
                format!("{} {}{}", quote_ident(&col.name), col.data_type, null)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let schema = schema_of(&planned).to_string();
        db.batch_execute(&format!(
            "CREATE TABLE {} ({})",
            qualified(&schema, &planned.name),
            columns
        ))
        .await
        .map_err(|e| res_err("failed to create table", e))?;

        planned.oid = fetch_oid(&db, &schema, &planned.name)
            .await
            .map_err(|e| res_err("failed to read table oid", e))?
            .ok_or_else(|| res_err("table disappeared", "not found after create"))?;
        Ok(planned)
    }

    async fn read(
        &self,
        _ctx: &mut Ctx,
        mut current: TableModel,
    ) -> Result<Option<TableModel>, ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        let schema = schema_of(&current).to_string();
        match fetch_oid(&db, &schema, &current.name)
            .await
            .map_err(|e| res_err("failed to read table", e))?
        {
            Some(oid) => {
                current.oid = oid;
                Ok(Some(current))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, _ctx: &mut Ctx, prior: TableModel) -> Result<(), ResourceError> {
        let db = self.pg.get().await.map_err(|e| res_err("connect", e))?;
        db.batch_execute(&format!(
            "DROP TABLE IF EXISTS {}",
            qualified(schema_of(&prior), &prior.name)
        ))
        .await
        .map_err(|e| res_err("failed to drop table", e))?;
        Ok(())
    }
}
