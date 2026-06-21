//! SQL plumbing shared by every resource: identifier/literal quoting and error
//! adapters.
//!
//! PostgreSQL cannot bind *identifiers* (table/role/column names) or DDL
//! literals (`CREATE ROLE … PASSWORD '…'`) as protocol parameters, so DDL has to
//! interpolate them as text. [`quote_ident`] and [`quote_literal`] are the
//! canonical, injection-safe way to do that — never build DDL with bare
//! `format!`. Ordinary `SELECT`/`WHERE` values *are* bound as `$1` parameters
//! everywhere in this crate.

use std::fmt::Display;

use terraform_runtime::{DataSourceError, ResourceError};

/// Quote a SQL identifier by doubling embedded double-quotes and wrapping the
/// whole thing in double-quotes — e.g. `weird"name` → `"weird""name"`. This is
/// the only safe way to splice a user-supplied name into DDL.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Quote a string literal by doubling embedded single-quotes and wrapping in
/// single-quotes. Assumes `standard_conforming_strings = on` (the PostgreSQL
/// default since 9.1), so backslashes are literal and need no escaping.
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Build a [`ResourceError`] from a context string and an underlying error.
pub fn res_err(context: &str, err: impl Display) -> ResourceError {
    ResourceError::new(context.to_string()).with_detail(err.to_string())
}

/// Build a [`DataSourceError`] from a context string and an underlying error.
pub fn ds_err(context: &str, err: impl Display) -> DataSourceError {
    DataSourceError::new(context.to_string()).with_detail(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_identifiers_and_escapes_embedded_quotes() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn quotes_literals_and_escapes_embedded_quotes() {
        assert_eq!(quote_literal("hunter2"), "'hunter2'");
        assert_eq!(quote_literal("O'Brien"), "'O''Brien'");
    }

    #[test]
    fn quoting_neutralizes_injection_attempts() {
        // A name trying to break out of the identifier stays inside it.
        let evil = "x\"; DROP TABLE users; --";
        let quoted = quote_ident(evil);
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        assert!(quoted.contains("\"\"")); // the embedded quote was doubled
    }
}
