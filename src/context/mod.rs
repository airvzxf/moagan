//! Context — `--context <ref>` plumbing for `moagan run`.
//!
//! A "context reference" is one of:
//!
//! - a `RunId` (UUID v7 of a previous run, picked up from
//!   `.runs/<id>/`),
//! - a filesystem path to a `.md` file or a directory full of them.
//!
//! `resolver` classifies the raw string the user typed and resolves
//! the run dir on disk. `loader` reads the contents into a
//! `LoadedContext` so downstream phases can prepend a context block
//! to the brief.
//!
//! Compliance: `proposal-02-rust.md` §3.4 (Reuso por `context`).
//! Phase J (v0.3 «tercera etapa», sub-fase J) wires the
//! `parent_run_id` + `shared_brief_hash` lineage into the SQLite
//! `runs` table and into the `manifest.json` sidecar.

pub mod loader;
pub mod resolver;

pub use loader::{ContextRefRecord, ContextScope, LoadedContext};
pub use resolver::{ContextRef, resolve, resolve_classify};
