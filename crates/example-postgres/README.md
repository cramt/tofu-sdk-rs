# `example-postgres` — a template PostgreSQL provider

A production-shaped Terraform/OpenTofu provider built with `tofu-sdk-rs`,
intended to be **copied as a starting point** for a real provider. Unlike
`example-aws` (backing-free) and `example-fs` (writes JSON files), this one talks
to a real PostgreSQL server over a pooled connection and manages genuine server
objects.

## What it manages

| Kind | Type | Notes |
|------|------|-------|
| Resource | `pg_role` | Login/superuser/createdb/createrole flags, **write-only** `password`, drift detection, import. |
| Resource | `pg_database` | Optional `owner`; owner change via `ALTER DATABASE`. |
| Resource | `pg_schema` | Optional `owner` (`AUTHORIZATION`). |
| Resource | `pg_extension` | Installable `version`, upgradable in place; computed `installed_version`. |
| Resource | `pg_table` | Columns as repeatable **nested blocks**; a `modify_plan` rule forces replacement when columns change. |
| Data source | `pg_database` | Singular lookup by name — projected from the **same model** as the resource. |
| Data source | `pg_roles` | Plural list of roles (dedicated, secret-free model). |

Every resource implements the full lifecycle (`create`/`read`/`update`/`delete`),
`read`-based drift detection (it returns `None` when the object has been removed
out of band, so Terraform plans to recreate it), `import`, and `validate`.

## Patterns worth copying

- **Connection pool as the configured meta.** `configure` builds a
  `deadpool-postgres` pool and pings it once so a bad endpoint fails fast with a
  clear diagnostic instead of erroring on the first resource. Every handler
  checks a connection out of the pool. See [`src/config.rs`](src/config.rs).
- **Environment fallback.** Each connection field resolves from the HCL value,
  then the standard `libpq` variable (`PGHOST`, `PGPORT`, `PGUSER`,
  `PGPASSWORD`, `PGDATABASE`, `PGSSLMODE`), then a default — so the provider
  works out of the box in a configured shell.
- **Injection-safe DDL.** PostgreSQL can't bind identifiers or DDL literals as
  parameters, so DDL interpolates them through `quote_ident` / `quote_literal`
  ([`src/sql.rs`](src/sql.rs)); ordinary `WHERE` values use `$1` bind params.
- **Write-only secrets.** `pg_role.password` is `#[facet(terraform::write_only)]`
  — the handler sees it at apply time, but the runtime nulls it out of every
  persisted state.
- **Defaults are applied handler-side, not via the schema.** Terraform only lets
  a provider inject a default for an attribute that is Optional *and* Computed,
  which this SDK cannot currently express (`Option` + `computed` collapses to
  read-only computed; a schema `default` on a plain optional makes OpenTofu
  reject the plan with *"planned value … for a non-computed attribute"*). So
  optional flags (`login`, `superuser`, `nullable`, `schema`, …) are plain
  `Option<T>` and the handler applies the default (`login.unwrap_or(true)`,
  `schema.unwrap_or("public")`, …).
- **Avoiding perpetual diffs.** Plain-optional attributes that the handler
  defaults (and `owner`) are **not** refreshed by `read` — an omitted value
  stays null in state instead of drifting to its catalog value every plan. Only
  the truly computed attributes (`oid`, `installed_version`) are refreshed. The
  data-source projection of `owner` is read back as computed.
- **`modify_plan` for replacement rules.** `pg_table` recreates the table when
  its `column` blocks change, rather than emitting `ALTER TABLE` (deliberately
  conservative about data); see the `modify_plan` impl in
  [`src/table.rs`](src/table.rs).

## Run it by hand

```bash
cargo build -p example-postgres   # builds target/debug/terraform-provider-postgres

DIR=$(mktemp -d)
ln -s "$PWD/target/debug/terraform-provider-postgres" "$DIR/terraform-provider-postgres"
cat > "$DIR/tofurc" <<EOF
provider_installation {
  dev_overrides { "example/postgres" = "$DIR" }
  direct {}
}
EOF
cat > "$DIR/main.tf" <<'EOF'
# Local name `pg` matches the `pg_*` resource type prefix.
terraform {
  required_providers {
    pg = { source = "example/postgres" }
  }
}

provider "pg" {
  host     = "localhost"
  port     = 5432
  username = "postgres"
  password = "postgres"
  database = "postgres"
}

resource "pg_role" "app" {
  name     = "app_user"
  login    = true
  password = "s3cret"
}
EOF

cd "$DIR"
TF_CLI_CONFIG_FILE="$DIR/tofurc" tofu apply   # against a running PostgreSQL
```

You can also drop the `provider` block entirely and rely on the `PG*`
environment variables.

## Tests

The contract suite ([`tests/`](tests)) spins up `postgres:16-alpine` in **Docker**,
points OpenTofu at the built provider via `dev_overrides`, wires the `PG*`
environment at the container, and runs a real `tofu test` (apply → assert →
destroy). It requires both `tofu`/`terraform` and Docker on `PATH` and runs as
part of `cargo test --workspace`:

```bash
nix develop --command bash -c 'cargo test -p example-postgres'
```

- `tests/tofu/lifecycle.tftest.hcl` — create every object, verify catalog-sourced
  computed values and both data sources, and confirm a re-plan is idempotent.
- `tests/tofu/update_replace.tftest.hcl` — an in-place `ALTER ROLE` update and a
  `force_new` table replacement.

## Enabling TLS

This template builds `tokio-postgres` with `NoTls`, so `configure` accepts only
`sslmode = "disable"`. To support real TLS, add a TLS connector
(e.g. `tokio-postgres-rustls`) and pass it to
`deadpool_postgres::Manager::from_config` instead of `NoTls`, mapping `sslmode`
to the connector's verification mode. The rest of the provider is unaffected.
