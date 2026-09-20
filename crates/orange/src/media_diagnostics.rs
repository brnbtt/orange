//! Media diagnostics facade.

mod decode_timeline;
mod operation;
mod progress;
mod webrtc_monitor;
mod writer;

pub(crate) use decode_timeline::track_decode_timeline;
pub(crate) use operation::{measure_operation, Operation};
pub(crate) use progress::{track_pad, MediaProgress, MediaStage};
pub(crate) use webrtc_monitor::{start_webrtc_diagnostics, DiagnosticsHandle};
pub(crate) use writer::{diagnostics_enabled, emit_diagnostic, DiagnosticWriter};

#[cfg(test)]
pub(crate) use writer::capture_diagnostics;
