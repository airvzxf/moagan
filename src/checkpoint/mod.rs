//! Human-in-the-loop checkpoint machinery.
//!
//! Phase D (V4 §5.14 + T01-06 §6.5) closes the "real interactivity"
//! of the system. The proposal-02 spec suggests `dialoguer::Input`,
//! but the AGENTS.md no-go list bans both `dialoguer` and `inquire`,
//! so the human prompts are implemented with the stdlib alone
//! (`std::io::stdin().read_line()`).
//!
//! The module exposes:
//!
//! - [`CheckpointKind`] — the closed enum of when a checkpoint may fire.
//! - [`Checkpoint`] — the question the pipeline asks.
//! - [`Resolution`] — what the user typed back.
//! - [`CheckpointOpts`] — runtime options (interactive / non-interactive /
//!   piped stdin / skip).
//! - [`ask`] — blocking read from stdin that writes the captured
//!   answer to `<run>/checkpoints/h_<NN>.json`.
//! - [`skip`] — same shape, but returns `Resolution::Approved` without
//!   touching stdin (non-interactive runs, `batch` mode, CI).

pub mod human;
pub mod modify_note;

pub use human::{Checkpoint, CheckpointKind, CheckpointOpts, Resolution, ask, skip};
pub use modify_note::{
    ModifyNote, load_modify_note, modify_note_path, persist_modify_note, prepend_to_prompt,
};
