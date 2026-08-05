//! Schema validator — JSON Schema Draft-07 checks for proposals
//! that ship a schema and a data document.
//!
//! Two artifact patterns are accepted:
//!
//! 1. **Paired** (the common case): the proposal carries one
//!    artifact with `language: "json-schema"` and a second with
//!    `language: "json"`. The data is validated against the
//!    schema.
//! 2. **Inline** (single artifact): `kind: "json-schema+data"`
//!    carrying a JSON object of the shape
//!    `{ "schema": <schema>, "data": <value> }`.
//!
//! The validator always returns Pass for parseable JSON even when
//! no schema is present (e.g. a proposal that only ships a config
//! file with no manifest), and Fail for invalid JSON or a
//! schema/data mismatch. A schema that is itself not a valid
//! JSON Schema Draft-07 is reported as Fail with the
//! `jsonschema` error list.
//!
//! Compliance: `proposal-01-concept.md` §5.8 ("JSON Schema. Parser
//! YAML/TOML. Validación de manifests."). YAML and TOML are
//! deferred — `serde_yaml` and `toml` are already in the
//! dependency tree but the user picked JSON Schema only for the
//! sub-fase C scope.

use crate::error::Result;
use crate::sandbox::Sandbox;

use super::{
    CodeArtifact, FailureKind, ValidationEvidence, ValidationFailure, ValidationStatus, Validator,
};

/// Schema validator. Stateless; reuse freely.
#[derive(Debug, Default, Clone, Copy)]
pub struct SchemaValidator;

impl SchemaValidator {
    /// Build a new instance.
    pub fn new() -> Self {
        Self
    }

    /// Language id for a JSON Schema document.
    pub const LANGUAGE: &'static str = "json-schema";
    /// Language id for a plain JSON document the schema validates.
    pub const LANGUAGE_JSON: &'static str = "json";

    /// Run the validator. Looks at every artifact on the proposal
    /// and emits a single evidence entry:
    /// - **Pass** when every paired data document satisfies its
    ///   schema and every standalone JSON document parses.
    /// - **Fail** when a schema is invalid, a data document is
    ///   invalid, or the JSON itself is not parseable.
    /// - **Skipped** when there is no schema and no JSON to
    ///   validate (e.g. a proposal without any of these artifacts).
    pub fn check(artifacts: &[CodeArtifact], _sandbox: &Sandbox) -> Result<ValidationEvidence> {
        // Pull out the schema and data candidates.
        let schema_artifact = artifacts
            .iter()
            .find(|a| a.language.eq_ignore_ascii_case(Self::LANGUAGE));
        let json_artifact = artifacts
            .iter()
            .find(|a| a.language.eq_ignore_ascii_case(Self::LANGUAGE_JSON));
        let inline_artifact = artifacts.iter().find(|a| a.kind == "json-schema+data");

        if schema_artifact.is_none() && json_artifact.is_none() && inline_artifact.is_none() {
            return Ok(ValidationEvidence::skipped(
                "schema",
                "no json-schema or json artifact present",
            ));
        }

        let mut evidence = ValidationEvidence {
            validator: "schema".into(),
            status: ValidationStatus::Pass,
            ..Default::default()
        };

        if let Some(schema_art) = schema_artifact {
            evidence
                .checks_run
                .push(format!("parse schema: {}", schema_art.kind));
        }
        if let Some(json_art) = json_artifact {
            evidence
                .checks_run
                .push(format!("parse json: {}", json_art.kind));
        }
        if let Some(inline) = inline_artifact {
            evidence
                .checks_run
                .push(format!("parse inline: {}", inline.kind));
        }

        // Validate every pair. Inline artifacts count as both a
        // schema and a data document bundled together.
        match validate_pairs(schema_artifact, json_artifact, inline_artifact) {
            Ok(report) => {
                evidence
                    .checks_run
                    .push(format!("validated {} pair(s)", report.pairs));
                if report.pairs == 0 {
                    // No actual validation happened (only standalone
                    // documents). If we got here, at least one
                    // JSON document parsed; record it as Pass.
                    evidence
                        .skipped_checks
                        .push("no schema/data pair to validate".into());
                }
            }
            Err(detail) => {
                evidence.status = ValidationStatus::Fail;
                evidence
                    .record_failure(ValidationFailure::new(FailureKind::SchemaViolation, detail));
            }
        }

        Ok(evidence)
    }
}

impl Validator for SchemaValidator {
    fn name(&self) -> &'static str {
        "schema"
    }

    fn validate(
        &self,
        _proposal: &crate::domain::Proposal,
        _sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence> {
        Ok(ValidationEvidence::skipped(
            "schema",
            "no source code attached; check called per-artifact",
        ))
    }
}

#[derive(Debug, Default)]
struct ValidationReport {
    pairs: usize,
}

fn validate_pairs(
    schema: Option<&CodeArtifact>,
    data: Option<&CodeArtifact>,
    inline: Option<&CodeArtifact>,
) -> std::result::Result<ValidationReport, String> {
    let mut report = ValidationReport::default();

    // Inline artifact (carries both schema and data).
    if let Some(inline) = inline {
        let bundle: serde_json::Value = serde_json::from_str(&inline.source)
            .map_err(|e| format!("inline json-schema+data artifact is not valid JSON: {e}"))?;
        let schema_value = bundle
            .get("schema")
            .ok_or_else(|| "inline artifact missing 'schema' field".to_string())?;
        let data_value = bundle
            .get("data")
            .ok_or_else(|| "inline artifact missing 'data' field".to_string())?;
        validate_value(schema_value, data_value)?;
        report.pairs += 1;
    }

    // Always parse every schema + data document we found. A
    // schema document that does not parse as JSON is a Fail;
    // a data document that does not parse is also a Fail, even
    // if no schema is present (the contract says the proposal
    // claims JSON so the bytes must be JSON).
    if let Some(s) = schema {
        let _: serde_json::Value = serde_json::from_str(&s.source)
            .map_err(|e| format!("schema artifact {} is not valid JSON: {e}", s.kind))?;
    }
    if let Some(d) = data {
        let _: serde_json::Value = serde_json::from_str(&d.source)
            .map_err(|e| format!("data artifact {} is not valid JSON: {e}", d.kind))?;
    }

    // Paired artifacts (schema + data).
    if let (Some(s), Some(d)) = (schema, data) {
        let schema_value: serde_json::Value = serde_json::from_str(&s.source)
            .map_err(|e| format!("schema artifact {} is not valid JSON: {e}", s.kind))?;
        let data_value: serde_json::Value = serde_json::from_str(&d.source)
            .map_err(|e| format!("data artifact {} is not valid JSON: {e}", d.kind))?;
        validate_value(&schema_value, &data_value)?;
        report.pairs += 1;
    }

    Ok(report)
}

fn validate_value(
    schema: &serde_json::Value,
    data: &serde_json::Value,
) -> std::result::Result<(), String> {
    let compiled = jsonschema::JSONSchema::compile(schema)
        .map_err(|e| format!("schema is not a valid JSON Schema: {e}"))?;
    let errors: Vec<_> = compiled
        .validate(data)
        .map(|_| Vec::new())
        .unwrap_or_else(|it| it.collect());
    if errors.is_empty() {
        Ok(())
    } else {
        let summary = errors
            .iter()
            .map(|e| format!("{}", e))
            .take(5)
            .collect::<Vec<_>>()
            .join("; ");
        Err(format!("data does not match schema: {summary}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Sandbox, SandboxConfig};

    fn sb() -> Sandbox {
        Sandbox::new(SandboxConfig::new()).unwrap()
    }

    const SCHEMA: &str = r#"{
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "id": {"type": "integer"},
            "name": {"type": "string"}
        },
        "required": ["id", "name"]
    }"#;

    const GOOD_DATA: &str = r#"{"id": 1, "name": "alice"}"#;
    const BAD_DATA: &str = r#"{"id": "not-a-number", "name": "alice"}"#;
    const MISSING_FIELD_DATA: &str = r#"{"id": 1}"#;

    #[test]
    fn paired_schema_and_data_pass() {
        let schema = CodeArtifact::new("schema.json", "json-schema", SCHEMA);
        let data = CodeArtifact::new("data.json", "json", GOOD_DATA);
        let ev = SchemaValidator::check(&[schema, data], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
        assert!(ev.checks_run.iter().any(|c| c.contains("validated 1 pair")));
    }

    #[test]
    fn paired_data_type_mismatch_fails() {
        let schema = CodeArtifact::new("schema.json", "json-schema", SCHEMA);
        let data = CodeArtifact::new("data.json", "json", BAD_DATA);
        let ev = SchemaValidator::check(&[schema, data], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        assert!(ev.failed_checks[0].contains("data does not match schema"));
    }

    #[test]
    fn paired_data_missing_required_field_fails() {
        let schema = CodeArtifact::new("schema.json", "json-schema", SCHEMA);
        let data = CodeArtifact::new("data.json", "json", MISSING_FIELD_DATA);
        let ev = SchemaValidator::check(&[schema, data], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
    }

    #[test]
    fn invalid_schema_fails() {
        let schema = CodeArtifact::new(
            "schema.json",
            "json-schema",
            r#"{"type": "this-is-not-a-valid-type"}"#,
        );
        let data = CodeArtifact::new("data.json", "json", GOOD_DATA);
        let ev = SchemaValidator::check(&[schema, data], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        assert!(ev.failed_checks[0].contains("not a valid JSON Schema"));
    }

    #[test]
    fn data_only_parses_to_pass() {
        let data = CodeArtifact::new("data.json", "json", GOOD_DATA);
        let ev = SchemaValidator::check(&[data], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
        assert!(
            ev.skipped_checks
                .iter()
                .any(|c| c.contains("no schema/data pair"))
        );
    }

    #[test]
    fn data_with_invalid_json_fails() {
        let data = CodeArtifact::new("data.json", "json", "{ this is not json");
        let ev = SchemaValidator::check(&[data], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        assert!(ev.failed_checks[0].contains("not valid JSON"));
    }

    #[test]
    fn empty_artifacts_is_skipped() {
        let ev = SchemaValidator::check(&[], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Skipped);
    }

    #[test]
    fn inline_schema_data_passes() {
        let inline = CodeArtifact::new(
            "json-schema+data",
            "json",
            format!(r#"{{"schema": {SCHEMA}, "data": {GOOD_DATA}}}"#),
        );
        let ev = SchemaValidator::check(&[inline], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
    }

    #[test]
    fn inline_schema_data_missing_field_fails() {
        let inline = CodeArtifact::new(
            "json-schema+data",
            "json",
            format!(r#"{{"schema": {SCHEMA}, "data": {MISSING_FIELD_DATA}}}"#),
        );
        let ev = SchemaValidator::check(&[inline], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
    }

    #[test]
    fn inline_artifact_missing_schema_field_fails() {
        let inline = CodeArtifact::new(
            "json-schema+data",
            "json",
            r#"{"data": {"id": 1, "name": "alice"}}"#,
        );
        let ev = SchemaValidator::check(&[inline], &sb()).unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        assert!(ev.failed_checks[0].contains("missing 'schema' field"));
    }

    #[test]
    fn validator_trait_returns_skipped() {
        let v = SchemaValidator::new();
        let p = crate::domain::Proposal::default();
        let e = v.validate(&p, None).unwrap();
        assert_eq!(e.status, ValidationStatus::Skipped);
    }
}
