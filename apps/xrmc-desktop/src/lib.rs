//! XR MultiCast native desktop app: casts many standalone VR headsets into one
//! lens-corrected, clearly labelled dashboard. Runs on Windows, macOS and Linux,
//! including small single-board computers with a touchscreen acting as the hub.
//!
//! * [`bridge`]: the hub (or demo) on a background tokio runtime; actions in, events out.
//! * [`decode`]: one decode worker per stream, layout detection.
//! * [`model`]: the UI's view model and the pure logic behind what is shown.
//! * [`view_report`]: debounced tile-size reports for resolution matching.
//! * [`ui`]: the egui interface; [`theme`]: colours, fonts, style.
//! * [`demo`]: synthetic headsets for testing and screenshots (`--demo N`).

pub mod bridge;
pub mod cli;
pub mod decode;
pub mod demo;
pub mod model;
pub mod offscreen;
pub mod platform;
pub mod theme;
pub mod ui;
pub mod view_report;
