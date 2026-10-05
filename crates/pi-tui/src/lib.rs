//! Native TUI renderer core (VED-307, ADR 0002).
//!
//! This crate is the native hot path of the interactive TUI: a cell grid, a
//! differential ANSI writer, the editor's grapheme/wrapping primitives, and the
//! `ctx.ui` dialogs. The JS `Component` bridge (running an extension's
//! `render()` in QuickJS and compositing its lines) composites into the same
//! [`buffer::Buffer`]; it is not part of this core.
//!
//! The renderer is transport-free so it can be unit-tested without a terminal:
//! callers build a [`buffer::Buffer`], diff it with [`render::diff`], and write
//! the returned bytes to their output.

pub mod buffer;
pub mod dialog;
pub mod editor;
pub mod render;

pub use buffer::{Buffer, Cell, Color, Style};
pub use dialog::{Dialog, DialogOutcome, DialogState};
pub use editor::{cursor_column, move_cursor, wrap_line, wrap_lines};
pub use render::{diff, Renderer};

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
