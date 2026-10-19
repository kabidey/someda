//! The desktop app's logic, free of any GUI type so it can be tested here.
//!
//! The window (`src/bin/someda-app`) renders [`state::AppState`] and feeds it
//! events; everything that decides something lives in these modules.

pub mod config;
pub mod deps;
pub mod interview;
pub mod libfetch;
pub mod project;
pub mod providers;
pub mod runner;
pub mod state;
pub mod symbols;
