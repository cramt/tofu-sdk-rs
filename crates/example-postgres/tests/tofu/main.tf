# Configuration under test for the `tofu test` contract suite. The provider is
# supplied via `dev_overrides` (see the Rust runner) and reads its connection
# settings from the `PG*` environment the runner points at the Docker container,
# so no provider block or `init` is needed.
#
# Names are variables so `run` blocks can drive in-place updates (toggling the
# role's superuser bit) and replacements (renaming the table → `force_new`).

# The local name must match the resource type prefix (`pg_*`), so the provider
# is declared as `pg` even though its source type is `postgres`.
terraform {
  required_providers {
    pg = {
      source = "example/postgres"
    }
  }
}

variable "role_name" {
  type    = string
  default = "app_user"
}

variable "role_superuser" {
  type    = bool
  default = false
}

variable "table_name" {
  type    = string
  default = "widgets"
}

# A login role with a write-only password (nulled out of state by the provider).
resource "pg_role" "app" {
  name      = var.role_name
  login     = true
  superuser = var.role_superuser
  password  = "s3cret-pw"
}

# A database owned by the role above (creates a create/destroy ordering edge).
resource "pg_database" "app" {
  name  = "appdb"
  owner = pg_role.app.name
}

# A schema in the connected database.
resource "pg_schema" "app" {
  name = "app"
}

# A contrib extension shipped with the postgres image.
resource "pg_extension" "crypto" {
  name = "pgcrypto"
}

# A table built from nested `column` blocks, living in the schema above.
resource "pg_table" "widgets" {
  name   = var.table_name
  schema = pg_schema.app.name

  column {
    name      = "id"
    data_type = "bigint"
    nullable  = false
  }

  column {
    name      = "label"
    data_type = "text"
  }
}

# Singular data source: look the database up by name and read back its owner.
data "pg_database" "app" {
  name = pg_database.app.name
}

# Plural data source: list every role.
data "pg_roles" "all" {}

output "role_oid" {
  value = pg_role.app.oid
}

output "db_owner_via_data_source" {
  value = data.pg_database.app.owner
}

output "role_count" {
  value = length(data.pg_roles.all.results)
}
