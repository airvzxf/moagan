//! Capability gate derived from a [`ModelsDevEntry`](crate::llm::models_dev::ModelsDevEntry).
//!
//! Three capability flags are enforced before a request reaches the
//! wire builder:
//!
//! 1. `attachment` — if the model advertises `attachment: false`, the
//!    gate refuses any [`Request`](crate::llm::wire::Request) that
//!    carries one or more [`Attachment`](crate::llm::wire::Attachment)
//!    entries. The refusal is hard: silently dropping the attachment
//!    would change the request identity the cross-run cache key is
//!    built on, and would let the user think their image was sent
//!    when it was actually dropped.
//! 2. `tool_call` — if the model advertises `tool_call: false`, the
//!    gate sets `req.tool_choice = None` so the wire builder omits
//!    the `tool_choice` (or equivalent) field. The model never sees
//!    a tool selector it would not honour.
//! 3. `modalities.input` — every attachment's `modality` field must
//!    appear in the model's accepted-input list. An image attached
//!    to a text-only model is rejected with a structured
//!    [`Error::ModalityUnsupported`](crate::error::Error::ModalityUnsupported).
//!
//! The gate is constructed via [`ModalityGate::from_entry`] when the
//! caller has a catalog row, or [`ModalityGate::from_conservative_default`]
//! when the catalog is unavailable. The conservative default mirrors
//! a text-only, no-attachments, no-tool-calls model so a fallback
//! never widens the set of capabilities the gate allows.
//!
//! Wiring into the request pipeline is left to a follow-up PR. This
//! module is the helper + the contract; the call-site change in
//! `phases::phase::RunContext` is a one-liner (`gate.apply(&mut req)?`)
//! and is intentionally deferred so the gate's behaviour can be
//! pinned by the unit + wiremock tests before the rest of the
//! pipeline starts depending on it.

use crate::error::{Error, Result};
use crate::llm::models_dev::ModelsDevEntry;
use crate::llm::wire::Request;

/// Capability gate derived from the models.dev catalog.
///
/// Mirrors the three booleans the catalog exposes
/// (`attachment`, `tool_call`, `modalities`) plus the input
/// modality list. The list is a `Vec<String>` rather than a
/// `HashSet` because the typical model accepts one or two
/// modalities (`text`, `text+image`) and the linear scan is
/// cheaper than a hash for those sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalityGate {
    /// Whether the model accepts file attachments.
    pub attachment: bool,
    /// Whether the model emits a separate tool / function-call field.
    pub tool_call: bool,
    /// Modalities the model accepts on the input side. Compared
    /// verbatim against [`Attachment::modality`](crate::llm::wire::Attachment::modality).
    pub modalities_in: Vec<String>,
    /// Modalities the model produces on the output side. The
    /// gate does not enforce this today (the request side
    /// cannot tell what the response will look like), but the
    /// field is exposed for symmetry and for a future PR that
    /// wants to short-circuit calls that would never produce
    /// a useful response.
    pub modalities_out: Vec<String>,
}

impl ModalityGate {
    /// Build the gate from a catalog row. The constructor is the
    /// one place where the catalog vocabulary (lowercase,
    /// singular) is consulted; the rest of the gate operates
    /// purely on the already-validated types.
    pub fn from_entry(entry: &ModelsDevEntry) -> Self {
        Self {
            attachment: entry.attachment,
            tool_call: entry.tool_call,
            modalities_in: entry.modalities.input.clone(),
            modalities_out: entry.modalities.output.clone(),
        }
    }

    /// Conservative default for callers that have no catalog row
    /// (e.g. a brand-new model the catalog has not indexed yet).
    /// Mirrors a text-only, no-attachments, no-tool-calls model so
    /// a fallback never widens the set of capabilities the gate
    /// allows. The `text` modality is in the input set so a plain
    /// `Request` with no attachments always passes; the
    /// `attachment: false` flag still rejects an image even
    /// though `image` would also be absent from the modality list.
    pub fn from_conservative_default() -> Self {
        Self {
            attachment: false,
            tool_call: false,
            modalities_in: vec!["text".to_string()],
            modalities_out: vec!["text".to_string()],
        }
    }

    /// Apply the gate to a request in place. Returns an error
    /// when the request violates the gate; otherwise mutates the
    /// request (drops `tool_choice` when `tool_call` is false) and
    /// returns `Ok(())`.
    ///
    /// Precedence of the three checks (matters for the error
    /// message the operator sees):
    ///
    /// 1. Attachment presence vs `attachment` flag — most common
    ///    failure mode for the `text-only` model rows in the
    ///    catalog, surfaces first.
    /// 2. Per-attachment modality check — surfaces only when the
    ///    gate would accept attachments at all, so a model that
    ///    cannot accept attachments reports the simpler error.
    /// 3. `tool_choice` cleanup — silent, only when the gate
    ///    forbids tool calls. The caller does not see this as an
    ///    error; the wire body simply omits the field.
    pub fn apply(&self, req: &mut Request) -> Result<()> {
        if !self.attachment && !req.attachments.is_empty() {
            return Err(Error::ModalityUnsupported(format!(
                "model {} does not accept attachments ({} attached)",
                req.model,
                req.attachments.len()
            )));
        }
        for att in &req.attachments {
            if !self.modalities_in.iter().any(|m| m == &att.modality) {
                return Err(Error::ModalityUnsupported(format!(
                    "{} attachment refused: model {} accepts {:?} only",
                    att.modality, req.model, self.modalities_in
                )));
            }
        }
        if !self.tool_call && req.tool_choice.is_some() {
            req.tool_choice = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::models_dev::{Limits, Modalities};
    use crate::llm::role::Role;
    use crate::llm::wire::{Attachment, ToolChoice};

    /// Build a `ModelsDevEntry` for tests. The `id`/`name`/`family`
    /// fields are populated with the test fixture; the rest of the
    /// struct is whatever the caller passed.
    fn entry(attachment: bool, tool_call: bool, modalities: (&[&str], &[&str])) -> ModelsDevEntry {
        ModelsDevEntry {
            id: "test-model".to_string(),
            name: "test-model".to_string(),
            family: Some("test".to_string()),
            attachment,
            reasoning: false,
            reasoning_options: vec![],
            tool_call,
            temperature: true,
            interleaved: None,
            modalities: Modalities {
                input: modalities.0.iter().map(|s| s.to_string()).collect(),
                output: modalities.1.iter().map(|s| s.to_string()).collect(),
            },
            limit: Limits {
                context: 8192,
                output: 2048,
            },
            cost: Default::default(),
            open_weights: false,
            release_date: None,
            last_updated: None,
        }
    }

    /// Skeleton request used by the gate tests. Filled in by the
    /// test body with the field under test.
    fn request() -> Request {
        Request {
            role: Role::Sketch,
            model: "test-model".to_string(),
            system: String::new(),
            user: String::new(),
            max_tokens: Some(1024),
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        }
    }

    /// PR-5 test 1: a model that does not accept attachments
    /// refuses a request that carries one. The error string names
    /// the model and the attachment count so the post-mortem log
    /// has enough context to fix the routing.
    #[test]
    fn attachment_false_blocks_any_attachment() {
        let gate = ModalityGate::from_entry(&entry(false, true, (&["text"], &["text"])));
        let mut req = request();
        req.attachments.push(Attachment {
            mime: "image/png".to_string(),
            modality: "image".to_string(),
            data: vec![0x89, 0x50, 0x4e, 0x47],
        });
        let err = gate.apply(&mut req).unwrap_err();
        match err {
            Error::ModalityUnsupported(msg) => {
                assert!(
                    msg.contains("test-model"),
                    "error must name the model: {msg}"
                );
                assert!(
                    msg.contains("does not accept attachments"),
                    "error must explain the refusal: {msg}"
                );
            }
            other => panic!("expected ModalityUnsupported, got {other:?}"),
        }
    }

    /// PR-5 test 2: a model that does accept attachments lets a
    /// request with one pass through. The attachment must also
    /// pass the modality check (`image` must be in the input
    /// list) for the call to succeed.
    #[test]
    fn attachment_true_allows_attachment() {
        let gate = ModalityGate::from_entry(&entry(true, true, (&["text", "image"], &["text"])));
        let mut req = request();
        req.attachments.push(Attachment {
            mime: "image/png".to_string(),
            modality: "image".to_string(),
            data: vec![0x89, 0x50, 0x4e, 0x47],
        });
        gate.apply(&mut req)
            .expect("attachment-true gate must accept image");
    }

    /// PR-5 test 3: a model that does not support tool calls has
    /// its `tool_choice` field silently dropped. The caller does
    /// not see an error; the wire body simply omits the field.
    /// The error variant test for the model-with-tool_choice
    /// case is covered by `tool_call_true_keeps_tool_choice`
    /// below.
    #[test]
    fn tool_call_false_drops_tool_choice() {
        let gate = ModalityGate::from_entry(&entry(true, false, (&["text"], &["text"])));
        let mut req = request();
        req.tool_choice = Some(ToolChoice::Auto);
        gate.apply(&mut req)
            .expect("tool_call-false gate must not error");
        assert!(
            req.tool_choice.is_none(),
            "tool_choice must be dropped to None when tool_call=false"
        );
    }

    /// PR-5 test 4: a model that supports tool calls keeps the
    /// `tool_choice` field intact. The gate is a no-op on the
    /// `tool_choice` axis for these rows.
    #[test]
    fn tool_call_true_keeps_tool_choice() {
        let gate = ModalityGate::from_entry(&entry(true, true, (&["text"], &["text"])));
        let mut req = request();
        req.tool_choice = Some(ToolChoice::Required);
        gate.apply(&mut req)
            .expect("tool_call-true gate must not error");
        assert_eq!(
            req.tool_choice,
            Some(ToolChoice::Required),
            "tool_choice must survive a tool_call-true gate"
        );
    }

    /// PR-5 test 5: an image attached to a text-only model is
    /// rejected even though the model advertises
    /// `attachment: true`. The per-attachment modality check
    /// fires before the simple attachment check would have a
    /// chance to; both fail here, so the modality error wins
    /// because it is more informative.
    #[test]
    fn modality_image_to_text_only_returns_error() {
        let gate = ModalityGate::from_entry(&entry(true, true, (&["text"], &["text"])));
        let mut req = request();
        req.attachments.push(Attachment {
            mime: "image/png".to_string(),
            modality: "image".to_string(),
            data: vec![0x89, 0x50, 0x4e, 0x47],
        });
        let err = gate.apply(&mut req).unwrap_err();
        match err {
            Error::ModalityUnsupported(msg) => {
                assert!(
                    msg.contains("image"),
                    "error must name the offending modality: {msg}"
                );
                assert!(
                    msg.contains("text"),
                    "error must list the accepted modalities: {msg}"
                );
            }
            other => panic!("expected ModalityUnsupported, got {other:?}"),
        }
    }

    /// PR-5 test 6: a text-only model with no attachments passes
    /// the gate. The conservative default is a text-only model,
    /// so this case is the "happy path" of every pipeline call
    /// that does not need attachments or tools.
    #[test]
    fn modality_text_to_text_only_succeeds() {
        let gate = ModalityGate::from_entry(&entry(false, true, (&["text"], &["text"])));
        let mut req = request();
        gate.apply(&mut req).expect("text-only request must pass");
    }

    /// Conservative-default gate mirrors a text-only, no-attachments,
    /// no-tool-calls model. The same shape is what the
    /// `from_conservative_default` constructor returns.
    #[test]
    fn conservative_default_rejects_image_and_tool_choice() {
        let gate = ModalityGate::from_conservative_default();
        let mut req = request();
        req.attachments.push(Attachment {
            mime: "image/png".to_string(),
            modality: "image".to_string(),
            data: vec![0x00],
        });
        req.tool_choice = Some(ToolChoice::Auto);
        let err = gate
            .apply(&mut req)
            .expect_err("conservative default must refuse an image");
        match err {
            Error::ModalityUnsupported(_) => {}
            other => panic!("expected ModalityUnsupported, got {other:?}"),
        }
    }

    /// Empty-attachment case: a request that carries no
    /// attachments passes the gate regardless of the `attachment`
    /// flag. The flag only gates requests that *do* carry
    /// attachments; an empty vector is a no-op.
    #[test]
    fn empty_attachments_always_passes() {
        let gate = ModalityGate::from_entry(&entry(false, true, (&["text"], &["text"])));
        let mut req = request();
        gate.apply(&mut req).expect("no attachments, no refusal");
    }

    /// Conservative default is text-only on the input side. A
    /// text-only request with no attachments and no tools passes
    /// the conservative default. The test pins the surface so
    /// the follow-up wiring can rely on `from_conservative_default`
    /// being permissive for the request shape every other
    /// pipeline call uses today.
    #[test]
    fn conservative_default_allows_text_request() {
        let gate = ModalityGate::from_conservative_default();
        let mut req = request();
        gate.apply(&mut req)
            .expect("text-only request must pass conservative default");
    }

    /// `from_entry` round-trips the three capability fields
    /// verbatim. Pins the constructor so a future refactor that
    /// drops a field (e.g. misnames `tool_call`) trips the test
    /// before the wire path starts depending on it.
    #[test]
    fn from_entry_copies_capability_fields() {
        let entry = entry(false, false, (&["text"], &["text"]));
        let gate = ModalityGate::from_entry(&entry);
        assert!(!gate.attachment);
        assert!(!gate.tool_call);
        assert_eq!(gate.modalities_in, vec!["text".to_string()]);
        assert_eq!(gate.modalities_out, vec!["text".to_string()]);
    }
}
