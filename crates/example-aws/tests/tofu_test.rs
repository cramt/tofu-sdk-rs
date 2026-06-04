//! End-to-end contract suite driven by the engine's native test framework.
//!
//! The real assertions live in `tests/tofu/*.tftest.hcl`; this Rust test is a
//! thin runner so the suite participates in `cargo test --workspace` (and CI).
//! It lays out a `dev_overrides` workspace pointing at the freshly built
//! provider, copies the fixtures in, and runs `tofu test`, which performs real
//! apply/plan/destroy cycles through the plugin protocol and evaluates the
//! `assert` blocks. A non-zero exit (any failed assertion) fails this test and
//! surfaces the engine output.

mod common;

use std::path::Path;

#[test]
fn tftest_suite_passes() {
    let engine = common::engine();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tofu");
    let ws = common::workspace_from_fixtures(&fixtures);

    let output = common::run(&engine, &["test", "-no-color"], &ws);
    common::assert_ok(&format!("{engine} test"), &output);
}
