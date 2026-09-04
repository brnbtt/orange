//! The design layer.
//!
//! Three files, one concern each:
//!
//! - [`theme`] — the tokens. Colour, type, metrics, motion. No elements.
//! - [`controls`] — reusable elements. Buttons, pills, cards, chrome.
//! - [`mark`] — the logo, its states, and the frame generation behind them.
//! - [`decor`] — the ambient layer. Grid, brackets, crosshairs.
//!
//! `view.rs` composes these into screens and owns every event handler. The
//! rule that keeps the split honest: nothing below this module may know what
//! screen it is on, and nothing above it may name a colour.

mod controls;
mod decor;
mod mark;
mod theme;

pub(crate) use controls::*;
pub(crate) use decor::*;
pub(crate) use mark::*;
pub(crate) use theme::*;
