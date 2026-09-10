#![forbid(unsafe_code)]
//! The format-independent half of a conversion: what a source file is, and
//! the pipeline every reader is driven through.
//!
//! A reader crate (`rvt-import`, `ifc-import`) knows one format and produces
//! a [`bim_core::BimModel`]. This crate knows none of them and owns what they
//! have in common, so that adding a format is adding a reader rather than
//! editing every caller.

pub mod classify;
pub mod format;

pub use classify::{element_type_for_source, ifc_entity_name, resolved_element_type};
pub use format::{Format, SNIFF_BYTES};
