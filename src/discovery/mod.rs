//! Discovery building blocks: the exploration matrix, the thesis
//! prompt payload, the run spec, the per-cell curation, and the
//! catalogue model and renderer that turn the theses into `final/`.
//!
//! The phases that wire them live in `src/phases/discover_*.rs`.

pub mod catalog;
pub mod cell_exclude;
pub mod curation;
pub mod epistemic_legacy;
pub mod matrix;
pub mod matrix_spec;
pub mod merge;
pub mod pause;
pub mod render;
pub mod resume;
pub mod run_spec;
pub mod thesis_prompt;
pub mod thesis_retry;

pub use matrix_spec::{DerivedDimensions, DimensionSpec, FacetSpec, MatrixSpec};
