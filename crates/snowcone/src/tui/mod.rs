//! Interactive TUI: tabbed views over every detected package manager,
//! with captured or terminal-takeover execution of mutations.

mod app;
mod event;
mod exec;
mod fetch;
mod keys;
mod modal;
mod packages;
mod policy;
mod pool;
mod tabs;
mod tasks;
mod trace;
mod ui;

pub use app::run;
