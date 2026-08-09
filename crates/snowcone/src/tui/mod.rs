//! The interactive TUI: discovers backends, then lets the user search,
//! browse, and inspect packages across all of them at once.

mod app;
mod event;
mod ui;

pub use app::run;
