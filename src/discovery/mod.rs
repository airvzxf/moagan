//! Discovery building blocks: the exploration matrix, the sketch
//! prompt payload, the run spec, and the catalogue model and
//! renderer that turn the sketches into `final/`.
//!
//! The phases that wire them live in `src/phases/discover_*.rs`.

pub mod catalog;
pub mod epistemic_legacy;
pub mod matrix;
pub mod matrix_spec;
pub mod pause;
pub mod render;
pub mod resume;
pub mod run_spec;
pub mod sketch_prompt;
pub mod sketch_retry;

pub use matrix_spec::{DerivedDimensions, DimensionSpec, FacetSpec, MatrixSpec};
