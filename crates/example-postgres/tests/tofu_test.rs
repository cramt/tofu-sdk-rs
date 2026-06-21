//! End-to-end contract suite: a real `tofu test` against a PostgreSQL server
//! running in Docker.
//!
//! The assertions live in `tests/tofu/*.tftest.hcl`; this runner starts the
//! container, lays out a `dev_overrides` workspace pointing at the freshly built
//! provider, copies the fixtures in, and runs `tofu test` with the `PG*`
//! environment wired to the container. `tofu test` performs real apply/plan/
//! destroy cycles through the plugin protocol — so every assertion exercises the
//! provider creating and tearing down genuine roles, databases, schemas,
//! extensions and tables.

mod common;

use std::path::Path;

#[test]
fn tftest_suite_passes_against_docker_postgres() {
    let pg = common::Postgres::start();
    let engine = common::engine();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tofu");
    let ws = common::workspace_from_fixtures(&fixtures);

    let output = common::run(&engine, &["test", "-no-color"], &ws, &pg);
    common::assert_ok(&format!("{engine} test"), &output);
}
