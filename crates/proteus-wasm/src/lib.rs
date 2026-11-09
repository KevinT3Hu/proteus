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
mod output;
pub mod process;
pub mod runtime;
pub mod secrets;
pub mod source;
pub mod storage;

pub use manifest::{FsGrant, Manifest, Permissions};
pub use output::printable;
pub use runtime::{Answer, Call, FromPlugin, Handle, Key, Pixels, Reply, Responder, Runtime, Sink, Spec, types};
pub use source::{Cache, Resolved, Source};

/// Characters that would let plugin-supplied text hide or reorder what the
/// user reads: controls, bidi overrides and isolates, and invisible format
/// characters (Unicode `Cf` that renders as nothing).
pub fn is_deceptive(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{061c}'
                | '\u{115f}'
                | '\u{1160}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{e0000}'..='\u{e007f}'
        )
}
