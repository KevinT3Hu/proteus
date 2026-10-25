//! The WASM plugin host (doc 05): package manifests and permissions,
//! explicit source resolution and caching, and the runtime that runs each
//! plugin instance sandboxed in its own thread.
//!
//! The GUI owns the terminal state: it receives the plugins' calls
//! ([`runtime::Call`]) through a [`runtime::Sink`], answers them on its
//! own thread, and sends events and surface input to [`runtime::Handle`]s.

pub mod accessibility;
pub mod files;
mod limits;
pub mod manifest;
pub mod net;
pub mod process;
pub mod runtime;
pub mod secrets;
pub mod source;
pub mod storage;

pub use manifest::{FsGrant, Manifest, Permissions};
pub use runtime::{Answer, Call, FromPlugin, Handle, Key, Pixels, Reply, Responder, Runtime, Sink, Spec, types};
pub use source::{Cache, Resolved, Source};
