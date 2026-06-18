//! The author-facing [`Function`] trait and its internal type erasure.
//!
//! A provider-defined function is **pure**: it maps argument values to a result
//! with no provider configuration, state, or side effects, and runs without
//! `ConfigureProvider`. Authors implement [`Function`] over a `Params` struct
//! (whose fields, in order, are the positional parameters) and an `Output` type
//! (the return). The runtime wraps each in an erased [`DynFunction`] that the
//! service dispatches to for `CallFunction`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use facet::{Facet, Type as FType, UserType};
use terraform_codec::{from_value, to_value};
use terraform_value::Value;

/// An error returned by a function call, surfaced to Terraform as a function
/// error (optionally pointing at the argument index that caused it).
#[derive(Debug, Clone)]
pub struct FunctionError {
    /// The error message.
    pub text: String,
    /// The zero-based index of the offending argument, if any.
    pub argument: Option<i64>,
}

impl FunctionError {
    /// Create an error with a message.
    pub fn new(text: impl Into<String>) -> Self {
        FunctionError {
            text: text.into(),
            argument: None,
        }
    }

    /// Point this error at the argument at `index`.
    pub fn at_argument(mut self, index: i64) -> Self {
        self.argument = Some(index);
        self
    }
}

impl From<&str> for FunctionError {
    fn from(s: &str) -> Self {
        FunctionError::new(s)
    }
}

impl From<String> for FunctionError {
    fn from(s: String) -> Self {
        FunctionError::new(s)
    }
}

/// A provider-defined function.
///
/// Implement this over a `Params` struct — each field is a positional parameter,
/// in declaration order — and an `Output` return type. Both are `#[derive(Facet)]`
/// types; the signature (parameter names/types and the return type) is reflected
/// from them. Register with [`ProviderBuilder::function`](crate::ProviderBuilder::function).
#[async_trait]
pub trait Function: Send + Sync + 'static {
    /// A struct whose fields are the function's positional parameters.
    type Params: Facet<'static> + Send + Sync;
    /// The function's return type.
    type Output: Facet<'static> + Send + Sync;

    /// Compute the result from the decoded parameters.
    async fn call(&self, params: Self::Params) -> Result<Self::Output, FunctionError>;
}

/// A **variadic** provider-defined function: fixed leading parameters plus a
/// final parameter accepting zero or more trailing arguments of one type.
///
/// Implement this over a `Params` struct (the leading positional parameters, as
/// in [`Function`]), a `VarArg` element type (the trailing arguments arrive as a
/// `Vec<VarArg>`), and an `Output` return type. The type system enforces the
/// shape the protocol requires — exactly one variadic, always last, uniform type
/// — so there is no marker to misplace. Register with
/// [`ProviderBuilder::function_variadic`](crate::ProviderBuilder::function_variadic).
#[async_trait]
pub trait VariadicFunction: Send + Sync + 'static {
    /// A struct whose fields are the leading positional parameters.
    type Params: Facet<'static> + Send + Sync;
    /// The element type of the trailing variadic arguments.
    type VarArg: Facet<'static> + Send + Sync;
    /// The function's return type.
    type Output: Facet<'static> + Send + Sync;

    /// Compute the result from the leading parameters and the trailing arguments.
    async fn call(
        &self,
        params: Self::Params,
        rest: Vec<Self::VarArg>,
    ) -> Result<Self::Output, FunctionError>;
}

/// Object-safe, value-oriented form of [`Function`] that the service dispatches
/// to. Receives one [`Value`] per positional argument (a variadic function's
/// trailing arguments included, flat) and returns the result.
#[async_trait]
pub trait DynFunction: Send + Sync {
    async fn call(&self, args: Vec<Value>) -> Result<Value, FunctionError>;
}

/// Wraps a typed [`Function`] as an erased [`DynFunction`].
pub struct FunctionAdapter<F: Function> {
    inner: F,
}

impl<F: Function> FunctionAdapter<F> {
    /// Erase `function` behind an `Arc<dyn DynFunction>`.
    pub fn erased(function: F) -> Arc<dyn DynFunction> {
        Arc::new(FunctionAdapter { inner: function })
    }
}

#[async_trait]
impl<F: Function> DynFunction for FunctionAdapter<F> {
    async fn call(&self, args: Vec<Value>) -> Result<Value, FunctionError> {
        let params = decode_params::<F::Params>(args)?;
        let output = self.inner.call(params).await?;
        to_value(&output).map_err(|e| FunctionError::new(format!("failed to encode result: {e}")))
    }
}

/// Wraps a typed [`VariadicFunction`] as an erased [`DynFunction`].
pub struct VariadicFunctionAdapter<F: VariadicFunction> {
    inner: F,
}

impl<F: VariadicFunction> VariadicFunctionAdapter<F> {
    /// Erase `function` behind an `Arc<dyn DynFunction>`.
    pub fn erased(function: F) -> Arc<dyn DynFunction> {
        Arc::new(VariadicFunctionAdapter { inner: function })
    }
}

#[async_trait]
impl<F: VariadicFunction> DynFunction for VariadicFunctionAdapter<F> {
    async fn call(&self, mut args: Vec<Value>) -> Result<Value, FunctionError> {
        // The flat argument list is [leading params…, trailing varargs…]; split it
        // at the number of leading parameters (the struct's field count).
        let lead = field_names::<F::Params>().len().min(args.len());
        let rest_values = args.split_off(lead);
        let params = decode_params::<F::Params>(args)?;

        let mut rest = Vec::with_capacity(rest_values.len());
        for (i, value) in rest_values.iter().enumerate() {
            rest.push(from_value::<F::VarArg>(value).map_err(|e| {
                FunctionError::new(format!("failed to decode variadic argument: {e}"))
                    .at_argument((lead + i) as i64)
            })?);
        }

        let output = self.inner.call(params, rest).await?;
        to_value(&output).map_err(|e| FunctionError::new(format!("failed to encode result: {e}")))
    }
}

/// Assemble positional `args` into the `Params` struct by zipping them with the
/// struct's fields (in declaration order) and decoding the resulting object.
fn decode_params<P: Facet<'static>>(args: Vec<Value>) -> Result<P, FunctionError> {
    let names = field_names::<P>();
    let mut object = BTreeMap::new();
    for (name, value) in names.into_iter().zip(args) {
        object.insert(name, value);
    }
    from_value(&Value::Object(object))
        .map_err(|e| FunctionError::new(format!("failed to decode arguments: {e}")))
}

/// The field names of a struct shape, in declaration order (empty for non-structs).
fn field_names<P: Facet<'static>>() -> Vec<String> {
    match &P::SHAPE.ty {
        FType::User(UserType::Struct(s)) => s
            .fields
            .iter()
            .map(|f| f.rename.unwrap_or(f.name).to_string())
            .collect(),
        _ => Vec::new(),
    }
}
