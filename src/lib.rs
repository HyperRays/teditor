//! A text editor built on a fixed cell grid, in layers that each build on the last:
//!
//! - [`renderer`] draws a display list of cells, glyphs, bitmaps and shapes with `wgpu`.
//! - [`terminal`] keeps front and back cell buffers and repaints only what changed.
//! - [`ui`] lays out tiled panes and routes input to application-drawn canvases.
//! - [`vt`] embeds libghostty-vt for the shell panel.
//! - [`tags`] finds where symbols are defined, running Universal Ctags as a separate program.
//! - [`editor`] is the application: documents, file tree, tabs, palette and shell panel.
//!
//! Run the benchmarks with `cargo bench`; they are in `benches/`.

#![warn(missing_docs)]

mod paths;
pub mod renderer;
pub mod tags;
pub mod terminal;
pub mod ui;
pub mod vt;

pub mod editor;
