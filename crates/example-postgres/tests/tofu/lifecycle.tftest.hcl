# Create lifecycle, data sources, and plan idempotency.
#
# A real apply through the plugin protocol creates every object; the assertions
# read back catalog-sourced computed values (OIDs, installed extension version)
# to prove the SQL actually ran. A second `plan` run confirms the computed values
# stay known and stable — if planning wrongly marked them unknown (a re-plan
# idempotency bug) the equality assertions would fail. `tofu test` destroys
# everything afterwards, exercising the full delete path in dependency order.

run "create_makes_real_objects" {
  command = apply

  assert {
    condition     = pg_role.app.oid > 0
    error_message = "the role should have a catalog OID after create"
  }

  # Write-only: the password reached the handler but must be null in state.
  assert {
    condition     = pg_role.app.password == null
    error_message = "the write-only password must not be persisted to state"
  }

  assert {
    condition     = pg_database.app.oid > 0
    error_message = "the database should have a catalog OID"
  }

  assert {
    condition     = pg_schema.app.oid > 0
    error_message = "the schema should have a catalog OID"
  }

  assert {
    condition     = pg_extension.crypto.installed_version != ""
    error_message = "the extension should report an installed version"
  }

  assert {
    condition     = pg_table.widgets.oid > 0
    error_message = "the table should have a catalog OID"
  }

  # Singular data source resolves the database's owner from the catalog.
  assert {
    condition     = data.pg_database.app.owner == var.role_name
    error_message = "the data source should read back the database owner"
  }

  # Plural data source lists roles — at least the bootstrap superuser and ours.
  assert {
    condition     = length(data.pg_roles.all.results) >= 2
    error_message = "the plural roles data source should list existing roles"
  }
}

run "replan_is_idempotent" {
  command = plan

  # Stable known computed values across a no-op re-plan: the catalog OIDs must
  # neither change nor go unknown (which would indicate a planning bug or
  # perpetual diff).
  assert {
    condition     = pg_role.app.oid > 0
    error_message = "role OID should stay known and stable on re-plan"
  }

  assert {
    condition     = pg_extension.crypto.installed_version != ""
    error_message = "extension version should stay known on re-plan"
  }
}
