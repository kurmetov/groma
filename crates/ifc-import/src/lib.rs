#![forbid(unsafe_code)]
//! IFC STEP parsing and conversion into openRVT's format-neutral BIM model.
//!
//! The parser and the model conversion are separate on purpose: callers can
//! report the time spent reading STEP independently from the time spent
//! resolving placements, representations, storeys and properties.

pub mod curve;
pub mod model;
pub mod place;
pub mod solid;
pub mod step;
pub mod units;

pub use model::{Import, Options, Read, convert, element_type};
pub use step::{Parsed, parse};
pub use units::Units;
