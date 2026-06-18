//! Terraform/OpenTofu value semantics.
//!
//! This crate models the two things that make Terraform values *not* ordinary
//! Rust values:
//!
//! 1. The `cty` type system ([`Type`]) — the structural type language Terraform
//!    uses to describe schemas and values on the wire.
//! 2. The known/unknown/null trichotomy ([`TfValue`]) — Terraform planning
//!    requires distinguishing a value that is *absent* (`Null`) from one that is
//!    *not yet computable* (`Unknown`).
//!
//! Neither concept depends on the Terraform plugin protocol; both are part of the
//! backend-agnostic core.

mod ty;
mod value;

pub use ty::{ObjectAttr, Type};
pub use value::{Number, Value};

/// A Terraform value that may be known, unknown, or null.
///
/// `Unknown` is **not** `Null`. `Null` means "this value is definitively
/// absent"; `Unknown` means "this value cannot be determined yet" (typically
/// because it depends on something that will only exist after apply). Collapsing
/// the two is the most common source of planning bugs, so they are distinct
/// variants here rather than an `Option`.
#[cfg_attr(feature = "facet", derive(facet::Facet))]
#[cfg_attr(feature = "facet", repr(u8))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TfValue<T> {
    /// A concrete, known value.
    Known(T),
    /// The value is not yet computable (will be resolved during apply).
    Unknown,
    /// The value is definitively absent.
    Null,
}

impl<T> TfValue<T> {
    /// Returns `true` if the value is [`TfValue::Known`].
    pub const fn is_known(&self) -> bool {
        matches!(self, TfValue::Known(_))
    }

    /// Returns `true` if the value is [`TfValue::Unknown`].
    pub const fn is_unknown(&self) -> bool {
        matches!(self, TfValue::Unknown)
    }

    /// Returns `true` if the value is [`TfValue::Null`].
    pub const fn is_null(&self) -> bool {
        matches!(self, TfValue::Null)
    }

    /// Returns a reference to the contained value, if known.
    pub const fn known(&self) -> Option<&T> {
        match self {
            TfValue::Known(v) => Some(v),
            _ => None,
        }
    }

    /// Maps `TfValue<T>` to `TfValue<U>`, preserving `Unknown`/`Null`.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> TfValue<U> {
        match self {
            TfValue::Known(v) => TfValue::Known(f(v)),
            TfValue::Unknown => TfValue::Unknown,
            TfValue::Null => TfValue::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_not_null() {
        let unknown: TfValue<i64> = TfValue::Unknown;
        let null: TfValue<i64> = TfValue::Null;
        assert_ne!(unknown, null);
        assert!(unknown.is_unknown());
        assert!(!unknown.is_null());
        assert!(null.is_null());
        assert!(!null.is_unknown());
    }

    #[test]
    fn map_preserves_non_known() {
        assert_eq!(TfValue::Known(2).map(|v| v * 2), TfValue::Known(4));
        assert_eq!(TfValue::<i64>::Unknown.map(|v| v * 2), TfValue::Unknown);
        assert_eq!(TfValue::<i64>::Null.map(|v| v * 2), TfValue::Null);
    }
}
