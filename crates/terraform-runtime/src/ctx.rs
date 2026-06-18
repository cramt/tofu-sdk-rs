//! The per-call handler context.
//!
//! Every resource and data-source handler receives `&mut Ctx`. It is the single
//! place a handler reaches the things that are *not* part of its typed model:
//!
//! - **success-path warnings** ([`Ctx::warn`]) — a diagnostic that rides
//!   alongside a *successful* result (deprecation notices, drift hints), unlike
//!   [`ResourceError::with_warning`](crate::ResourceError::with_warning) which
//!   needs an accompanying error;
//! - **private state** ([`Ctx::private`] / [`Ctx::set_private`]) — opaque bytes
//!   the provider persists across a resource's operations (Terraform stores them
//!   but never inspects them);
//! - **cancellation** ([`Ctx::is_cancelled`] / [`Ctx::cancelled`]) — tripped by
//!   `StopProvider`, to abort long work promptly.
//!
//! The service injects a `Ctx` ambiently around each dispatch (a task-local,
//! mirroring the existing cancellation scope) and reads the accumulated outputs
//! back afterwards. The erased [`DynResource`](crate::DynResource) /
//! [`DynDataSource`](crate::DynDataSource) seam is *unchanged*: the adapter pulls
//! the ambient context with [`current_ctx`] and passes it to the typed handler,
//! so non-Rust frontends (the Node binding) need no change.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::resource::Diag;

tokio::task_local! {
    /// The ambient handler context for the in-flight dispatch, installed by
    /// [`with_ctx`] and read by the adapter via [`current_ctx`].
    static CTX: Ctx;
}

/// The side effects a handler accumulates, read back by the service once the
/// handler returns.
#[derive(Default)]
struct CtxSink {
    warnings: Vec<Diag>,
    private_out: Option<Vec<u8>>,
}

/// The context handed to every handler call as `&mut Ctx`.
///
/// Cloning is cheap (shared handles) and clones observe the same accumulated
/// state — the service relies on that to read a handler's warnings and new
/// private state after the call.
#[derive(Clone)]
pub struct Ctx {
    cancel: CancellationToken,
    private_in: Arc<[u8]>,
    sink: Arc<Mutex<CtxSink>>,
}

impl Ctx {
    /// Build a context for a dispatch carrying `private_in` (the resource's
    /// stored private state) and `cancel` (tripped by `StopProvider`).
    pub(crate) fn new(private_in: impl Into<Arc<[u8]>>, cancel: CancellationToken) -> Self {
        Ctx {
            cancel,
            private_in: private_in.into(),
            sink: Arc::new(Mutex::new(CtxSink::default())),
        }
    }

    /// A detached context for a handler invoked outside a dispatch (e.g. a unit
    /// test calling an adapter directly): warnings/private go nowhere, nothing is
    /// cancelled, and there is no prior private state.
    pub(crate) fn detached() -> Self {
        Ctx::new(Vec::new(), CancellationToken::new())
    }

    /// Emit a warning diagnostic alongside a successful result.
    pub fn warn(&mut self, summary: impl Into<String>, detail: impl Into<String>) {
        self.warning(Diag::warning(summary, detail));
    }

    /// Emit a prebuilt warning [`Diag`] (e.g. one pointed at an attribute with
    /// [`Diag::at`]).
    pub fn warning(&mut self, warning: Diag) {
        self.lock().warnings.push(warning);
    }

    /// The resource's stored private state (empty when there is none).
    pub fn private(&self) -> &[u8] {
        &self.private_in
    }

    /// Replace the resource's private state to persist for its next operation.
    pub fn set_private(&mut self, bytes: impl Into<Vec<u8>>) {
        self.lock().private_out = Some(bytes.into());
    }

    /// Whether `StopProvider` has been received — poll this in long loops.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolves when the in-flight operation is cancelled; `select!` on it to
    /// abort a long await promptly.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await
    }

    /// The raw cancellation token, for handlers that need to hand it onward.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CtxSink> {
        self.sink.lock().expect("handler ctx sink poisoned")
    }

    /// Drain the accumulated side effects.
    fn into_outputs(self) -> CtxOutputs {
        let mut sink = self.lock();
        CtxOutputs {
            warnings: std::mem::take(&mut sink.warnings),
            private_out: sink.private_out.take(),
        }
    }
}

/// What a handler produced beyond its return value, merged into the response.
#[derive(Default)]
pub(crate) struct CtxOutputs {
    pub warnings: Vec<Diag>,
    pub private_out: Option<Vec<u8>>,
}

/// Run `fut` with `ctx` installed as the ambient handler context, returning the
/// future's output together with the context's accumulated outputs.
pub(crate) async fn with_ctx<T>(ctx: Ctx, fut: impl Future<Output = T>) -> (T, CtxOutputs) {
    let probe = ctx.clone();
    let out = CTX.scope(ctx, fut).await;
    (out, probe.into_outputs())
}

/// The ambient context for the currently-executing handler, or a detached one
/// when called outside a dispatch.
pub(crate) fn current_ctx() -> Ctx {
    CTX.try_with(Ctx::clone).unwrap_or_else(|_| Ctx::detached())
}
