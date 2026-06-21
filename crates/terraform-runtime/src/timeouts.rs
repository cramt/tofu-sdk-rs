//! `timeouts {}` block support (roadmap 3.4) — the conventional per-operation
//! deadline block (`create`/`read`/`update`/`delete`), read from the resource value
//! and enforced by wrapping each handler dispatch in a [`tokio::time::timeout`].
//!
//! Unlike SDKv2 — where `timeouts` is a bolted-on schema concept — here it is just a
//! **nested block** the author embeds on the model (`#[facet(terraform::block)]
//! timeouts: Option<Timeouts>`). Reflection emits it like any other optional single
//! block; the runtime then reads the relevant duration off the dynamic [`Value`] at
//! apply/read time (so the typed handler need not look at it) and bounds the
//! operation. A handler that overruns yields a clean error diagnostic, never a hung
//! plugin.
//!
//! Durations use Go's `time.ParseDuration` spelling (`"30s"`, `"5m"`, `"1h30m"`,
//! `"500ms"`) — what Terraform users already write. Absent/blank/zero/unparseable →
//! no bound (the operation runs unbounded, matching Terraform's default).

use std::collections::BTreeMap;
use std::time::Duration;

use facet::Facet;
use terraform_value::Value;

use crate::resource::{Diag, Diagnostics};

/// The conventional block name authors give the field.
const BLOCK: &str = "timeouts";

/// The standard `timeouts {}` block. Embed it as an optional nested block on a
/// resource model to let users set per-operation deadlines:
///
/// ```ignore
/// #[derive(Facet)]
/// #[facet(terraform::resource("aws_thing"))]
/// struct Thing {
///     name: String,
///     #[facet(terraform::block)]
///     timeouts: Option<terraform_runtime::Timeouts>,
/// }
/// ```
///
/// Each field is an optional Go-style duration string. The runtime reads and
/// enforces them; the handler can ignore the field entirely.
#[derive(Facet, Default, Debug, Clone)]
#[allow(dead_code)]
pub struct Timeouts {
    /// Deadline for `ApplyResourceChange` create.
    pub create: Option<String>,
    /// Deadline for `ReadResource`.
    pub read: Option<String>,
    /// Deadline for `ApplyResourceChange` update.
    pub update: Option<String>,
    /// Deadline for `ApplyResourceChange` delete.
    pub delete: Option<String>,
}

/// Read the configured deadline for `operation` (`"create"`/`"read"`/`"update"`/
/// `"delete"`) from a resource value's `timeouts` block, if present and parseable.
/// `value` is the create/update *planned* state, the delete *prior* state, or the
/// read *current* state.
pub fn for_operation(value: &Value, operation: &str) -> Option<Duration> {
    let Value::Object(fields) = value else {
        return None;
    };
    let entries = block_entries(fields.get(BLOCK)?)?;
    match entries.get(operation) {
        Some(Value::String(s)) => parse_go_duration(s),
        _ => None,
    }
}

/// A single nested block arrives as an object; defensively also accept a
/// single-element list/set (how a repeated block would be carried) so the lookup is
/// robust to the nesting mode.
fn block_entries(block: &Value) -> Option<&BTreeMap<String, Value>> {
    match block {
        Value::Object(o) | Value::Map(o) => Some(o),
        Value::List(items) | Value::Set(items) => match items.first() {
            Some(Value::Object(o)) => Some(o),
            _ => None,
        },
        _ => None,
    }
}

/// The diagnostic returned when an operation exceeds its deadline.
pub fn timed_out(operation: &str, limit: Duration) -> Diagnostics {
    vec![Diag::error(
        format!("{operation} timed out"),
        format!(
            "the {operation} operation exceeded its configured timeout of {}",
            humanize(limit)
        ),
    )]
}

/// Run `fut` under an optional deadline. With `None` it runs unbounded; with
/// `Some(limit)` an overrun resolves to [`timed_out`] instead of the handler's
/// result. Sits *inside* the panic-guard/ctx scope, so cancellation and warnings
/// still apply up to the deadline.
pub async fn bounded<T, F>(
    operation: &'static str,
    limit: Option<Duration>,
    fut: F,
) -> Result<T, Diagnostics>
where
    F: std::future::Future<Output = Result<T, Diagnostics>>,
{
    match limit {
        None => fut.await,
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(result) => result,
            Err(_) => Err(timed_out(operation, d)),
        },
    }
}

/// Parse a Go-style duration (`time.ParseDuration`): a sequence of
/// `<number><unit>` terms (`"1h30m"`, `"500ms"`, `"1.5s"`). Supported units:
/// `ns`, `us`/`µs`/`μs`, `ms`, `s`, `m`, `h`. Returns `None` for empty or malformed
/// input; `"0"` is a valid zero.
fn parse_go_duration(input: &str) -> Option<Duration> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if s == "0" {
        return Some(Duration::ZERO);
    }

    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total = Duration::ZERO;
    while i < bytes.len() {
        // Number (digits and at most one fractional point).
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == start {
            return None;
        }
        let value: f64 = s[start..i].parse().ok()?;

        // Unit (the run of non-numeric bytes up to the next term).
        let unit_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_digit() && bytes[i] != b'.' {
            i += 1;
        }
        let seconds = match &s[unit_start..i] {
            "ns" => value * 1e-9,
            "us" | "µs" | "μs" => value * 1e-6,
            "ms" => value * 1e-3,
            "s" => value,
            "m" => value * 60.0,
            "h" => value * 3600.0,
            _ => return None,
        };
        if !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        total += Duration::from_secs_f64(seconds);
    }
    Some(total)
}

/// Render a `Duration` back for a diagnostic, preferring whole seconds.
fn humanize(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1.0 && secs.fract() == 0.0 {
        format!("{}s", secs as u64)
    } else if secs >= 1.0 {
        format!("{secs}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_go_durations() {
        assert_eq!(parse_go_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_go_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_go_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(
            parse_go_duration("1h30m"),
            Some(Duration::from_secs(5400)),
            "compound terms sum"
        );
        assert_eq!(parse_go_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration("0"), Some(Duration::ZERO));
    }

    #[test]
    fn rejects_malformed_durations() {
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("   "), None);
        assert_eq!(parse_go_duration("30"), None, "a bare number needs a unit");
        assert_eq!(parse_go_duration("5x"), None, "unknown unit");
        assert_eq!(parse_go_duration("abc"), None);
    }

    fn with_timeouts(op: &str, dur: &str) -> Value {
        let mut inner = BTreeMap::new();
        inner.insert(op.to_string(), Value::String(dur.to_string()));
        let mut outer = BTreeMap::new();
        outer.insert(BLOCK.to_string(), Value::Object(inner));
        Value::Object(outer)
    }

    #[test]
    fn reads_the_operations_duration_from_the_block() {
        let v = with_timeouts("create", "10m");
        assert_eq!(for_operation(&v, "create"), Some(Duration::from_secs(600)));
        // A different operation with no entry → no bound.
        assert_eq!(for_operation(&v, "delete"), None);
    }

    #[test]
    fn reads_from_a_single_element_block_list() {
        // Robust to the block arriving as a one-element list (set/list nesting).
        let mut inner = BTreeMap::new();
        inner.insert("update".to_string(), Value::String("2m".into()));
        let mut outer = BTreeMap::new();
        outer.insert(BLOCK.to_string(), Value::List(vec![Value::Object(inner)]));
        let v = Value::Object(outer);
        assert_eq!(for_operation(&v, "update"), Some(Duration::from_secs(120)));
    }

    #[test]
    fn no_block_or_blank_means_no_bound() {
        assert_eq!(
            for_operation(&Value::Object(BTreeMap::new()), "create"),
            None
        );
        let v = with_timeouts("create", "  ");
        assert_eq!(for_operation(&v, "create"), None);
    }

    #[tokio::test]
    async fn bounded_times_out_a_slow_future() {
        let res: Result<i32, Diagnostics> =
            bounded("create", Some(Duration::from_millis(10)), async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(1)
            })
            .await;
        let diags = res.expect_err("should time out");
        assert!(diags[0].summary.contains("create timed out"), "{diags:?}");
    }

    #[tokio::test]
    async fn bounded_passes_through_a_fast_future_and_no_limit() {
        let fast: Result<i32, Diagnostics> =
            bounded("create", Some(Duration::from_secs(30)), async { Ok(7) }).await;
        assert_eq!(fast.unwrap(), 7);
        let unbounded: Result<i32, Diagnostics> = bounded("read", None, async { Ok(9) }).await;
        assert_eq!(unbounded.unwrap(), 9);
    }
}
