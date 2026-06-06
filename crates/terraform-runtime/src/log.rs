//! A minimal [`tracing::Subscriber`] that emits Terraform's structured JSON log
//! format on **stderr**, so provider logs appear under `TF_LOG`.
//!
//! go-plugin captures the plugin's stderr; when a line is JSON with hclog's
//! `@`-prefixed keys (`@level`, `@message`, `@module`, `@timestamp`) Terraform
//! folds it into its own log stream. stdout is reserved for the handshake line
//! and the gRPC channel — nothing here ever writes there.
//!
//! This is deliberately tiny: spans are no-ops (we only render events), and we
//! build the JSON by hand to avoid a `serde`/`tracing-subscriber` dependency.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// Install structured provider logging when `TF_LOG_PROVIDER` or `TF_LOG`
/// requests it; a no-op (and silent) otherwise. Idempotent: a second call, or a
/// global subscriber already set by the host application, is ignored.
pub fn init() {
    let Some(level) = requested_level() else {
        return;
    };
    let _ = tracing::subscriber::set_global_default(TerraformLogger::new(level));
}

/// The max level requested via `TF_LOG_PROVIDER` (preferred) or `TF_LOG`.
fn requested_level() -> Option<Level> {
    let raw = std::env::var("TF_LOG_PROVIDER")
        .or_else(|_| std::env::var("TF_LOG"))
        .ok()?;
    parse_level(&raw)
}

/// Parse a Terraform log level (case-insensitive). Matching Terraform, an
/// unrecognized but non-empty value falls back to `TRACE`; an empty value
/// disables logging.
fn parse_level(raw: &str) -> Option<Level> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "" => None,
        "DEBUG" => Some(Level::DEBUG),
        "INFO" => Some(Level::INFO),
        "WARN" | "WARNING" => Some(Level::WARN),
        "ERROR" => Some(Level::ERROR),
        // "TRACE", "JSON", and anything else Terraform treats as trace.
        _ => Some(Level::TRACE),
    }
}

/// A numeric verbosity so we can compare levels without depending on
/// `tracing`'s (easy-to-misread) `Ord` impl. Higher = more verbose.
fn verbosity(level: &Level) -> u8 {
    match *level {
        Level::ERROR => 1,
        Level::WARN => 2,
        Level::INFO => 3,
        Level::DEBUG => 4,
        Level::TRACE => 5,
    }
}

/// hclog's lowercase level names.
fn level_str(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

struct TerraformLogger {
    /// Maximum verbosity to emit (inclusive).
    max: u8,
    /// Span id source. Spans are otherwise unused, but `new_span` must hand back
    /// a unique non-zero id.
    next_id: AtomicU64,
}

impl TerraformLogger {
    fn new(level: Level) -> Self {
        TerraformLogger {
            max: verbosity(&level),
            next_id: AtomicU64::new(1),
        }
    }
}

impl Subscriber for TerraformLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        verbosity(metadata.level()) <= self.max
    }

    fn new_span(&self, _attrs: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let meta = event.metadata();
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let ts = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        let line = build_line(
            level_str(meta.level()),
            meta.target(),
            &ts,
            &visitor.message,
            &visitor.fields,
        );
        // Logging must never panic the provider: a single best-effort write.
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{line}");
    }

    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}
}

/// Collects an event's fields: the special `message` field as plain text, every
/// other field as a `(name, json-value)` pair where the value is already a valid
/// JSON token (quoted string, or bare number/bool).
#[derive(Default)]
struct JsonVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl JsonVisitor {
    fn push(&mut self, field: &Field, json_value: String) {
        self.fields.push((field.name().to_string(), json_value));
    }
}

impl Visit for JsonVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = rendered;
        } else {
            self.push(field, json_string(&rendered));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.push(field, json_string(value));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        // Non-finite floats aren't valid JSON; fall back to a quoted string.
        if value.is_finite() {
            self.push(field, value.to_string());
        } else {
            self.push(field, json_string(&value.to_string()));
        }
    }
}

/// Build one hclog JSON line: the `@`-prefixed core fields first, then any extra
/// event fields. Pure (timestamp passed in) so it is unit-testable.
fn build_line(
    level: &str,
    module: &str,
    timestamp: &str,
    message: &str,
    fields: &[(String, String)],
) -> String {
    let mut s = String::with_capacity(96);
    s.push('{');
    s.push_str("\"@level\":");
    s.push_str(&json_string(level));
    s.push_str(",\"@message\":");
    s.push_str(&json_string(message));
    s.push_str(",\"@module\":");
    s.push_str(&json_string(module));
    s.push_str(",\"@timestamp\":");
    s.push_str(&json_string(timestamp));
    for (key, value) in fields {
        s.push(',');
        s.push_str(&json_string(key));
        s.push(':');
        s.push_str(value);
    }
    s.push('}');
    s
}

/// JSON-encode a string: surrounding quotes and the mandatory escapes.
fn json_string(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len() + 2);
    s.push('"');
    for c in raw.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_line_emits_valid_terraform_json() {
        let line = build_line(
            "info",
            "terraform_runtime::service",
            "2026-06-06T00:00:00Z",
            "serving provider",
            &[
                ("type_name".to_string(), json_string("aws_s3_bucket")),
                ("count".to_string(), "3".to_string()),
            ],
        );

        // Parses as JSON (validity) — same path the codec uses elsewhere.
        let _: facet_value::Value =
            facet_json::from_slice(line.as_bytes()).expect("log line is valid JSON");

        // Carries hclog's core keys with the expected values.
        assert!(line.contains(r#""@level":"info""#), "{line}");
        assert!(line.contains(r#""@message":"serving provider""#), "{line}");
        assert!(
            line.contains(r#""@module":"terraform_runtime::service""#),
            "{line}"
        );
        assert!(
            line.contains(r#""@timestamp":"2026-06-06T00:00:00Z""#),
            "{line}"
        );
        // Extra fields are appended: string quoted, number bare.
        assert!(line.contains(r#""type_name":"aws_s3_bucket""#), "{line}");
        assert!(line.contains(r#""count":3"#), "{line}");
    }

    #[test]
    fn json_string_escapes_control_and_quotes() {
        assert_eq!(json_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(json_string("line\nbreak"), r#""line\nbreak""#);
    }

    #[test]
    fn parse_level_matches_terraform_semantics() {
        assert_eq!(parse_level("trace"), Some(Level::TRACE));
        assert_eq!(parse_level("DEBUG"), Some(Level::DEBUG));
        assert_eq!(parse_level("warn"), Some(Level::WARN));
        assert_eq!(parse_level("json"), Some(Level::TRACE));
        assert_eq!(parse_level("nonsense"), Some(Level::TRACE));
        assert_eq!(parse_level(""), None);
    }
}
