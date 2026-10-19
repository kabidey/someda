//! someda — a clean-room KiCad-compatible EDA core.
//!
//! Implements the architecture patterns documented in the SomTronics
//! AI-EDA study: s-expression board model, DRC engine, net classes,
//! geometry, and the verify-before-done ("honest-done") repair loop.

pub mod sexp;
pub mod board;
pub mod drc;
pub mod netclass;
pub mod geometry;
pub mod report;
pub mod schematic;
pub mod verified;
pub mod netlist;
pub mod stackup;
pub mod gateway;
pub mod kicad_cli;
pub mod orchestrator;
pub mod placer;
pub mod router;
pub mod boardplan;
pub mod bringup;
pub mod formulas;
