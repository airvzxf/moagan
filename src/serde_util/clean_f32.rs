//! Serialize `f32` as its shortest round-trip decimal (Ryu) so
//! sidecars carry `0.1` instead of `0.10000000149011612`. The
//! helper is `pub` so any downstream `serde` derive can adopt
//! it without duplicating the conversion.
//!
//! ## Why this exists
//!
//! `Rust 1.55+` already formats `f32` via Ryu (so
//! `format!("{0.1_f32}")` is `"0.1"`), but the **TOML encoder
//! used by the runtime** does not. `toml_edit::ValueSerializer`
//! widens every `f32` to `f64` via `serialize_f32(v) →
//! serialize_f64(v as f64)`. The `as f64` cast preserves the
//! `f32` bit pattern verbatim (so `0.1_f32` becomes
//! `0.10000000149011612_f64`), and TOML emits the latter as a
//! 17-digit decimal. JSON encoders (`serde_json`) already use
//! Ryu on `f32` directly, so the helper is mostly a no-op for
//! JSON sidecars — but applying it explicitly is cheap,
//! future-proofs against a backend that widens `f32 → f64`
//! before emitting, and keeps TOML / JSON wire shapes uniform.
//!
//! Three variants are exposed:
//!
//! - [`vec`] — for `[f32]` / `Vec<f32>` fields like the
//!   temperature probe sidecar.
//! - [`scalar`] — for single `f32` fields (judge scores, cluster
//!   cohesion, …). Used by `Ranking::score`,
//!   `Cluster::cohesion`, `AdversaryReport::score_delta`, …
//! - [`opt_scalar`] — for `Option<f32>` fields where `None` should
//!   round-trip as JSON `null` and `Some(v)` should reuse the
//!   same Ryu helper. Used by `Ranking::stability_sigma`.
//!
//! Deserialisation uses the default `f32` deserialiser for every
//! variant: the operator can keep writing `score = 0.85` in TOML
//! or `"score": 0.85` in JSON, and the legacy long form
//! (`0.85000002384…`) from a pre-v0.12.4 sidecar lands on the same
//! `0.85_f32` bits. Sidecars are therefore forward- and
//! backward-compatible across the v0.12.4 migration.
//!
//! ## NaN / infinity
//!
//! `clean_float(f32::NAN)` returns `f64::NAN`. `serde_json`
//! serialises NaN as JSON `null` and the subsequent
//! deserialisation fails with `invalid type: null, expected
//! f32` — a deliberate guardrail against silent data corruption
//! on an upstream that emits a pathological value. Operators
//! who legitimately need NaN should switch the field to
//! `Option<f32>` and handle the `None` arm explicitly. The
//! TOML encoder rejects `nan` outright (per the TOML spec).

use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serializer};

/// Convert an `f32` to its shortest round-trip decimal,
/// represented as `f64` so it survives the TOML encoder's
/// `f32 → f64` widening without leaking the original `f32`
/// bit pattern into a 17-digit decimal. Operator-written `0.1`
/// round-trips as `0.1`; native `0.1_f32` (which is the bit
/// pattern `0x3dcccccd`) round-trips as `0.1` because Ryu
/// (`format!("{f32}")`, Rust ≥1.55) emits the shortest string
/// that parses back to the same `f32` bits — *not* the
/// TOML-side `0.10000000149011612` widening artefact.
pub fn clean_float(t: f32) -> f64 {
    format!("{t}")
        .parse::<f64>()
        .expect("Ryu produces a valid f64 representation for any f32")
}

pub mod vec {
    //! `Vec<f32>` / `[f32]` (e.g. a temperature list).
    use super::*;

    /// Serialise each element through [`super::clean_float`] so
    /// the emitted sequence is the shortest round-trip
    /// decimal per element.
    pub fn serialize<S>(temps: &[f32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(temps.len()))?;
        for &t in temps {
            seq.serialize_element(&clean_float(t))?;
        }
        seq.end()
    }

    /// Default `Vec<f32>` deserialiser — accepts the clean Ryu
    /// form and the legacy `Display::fmt` form with no migration
    /// shim needed.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<f32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<f32>::deserialize(deserializer)
    }
}

pub mod scalar {
    //! `f32` simple (a single scalar field).
    use super::*;

    /// Serialise the value through [`super::clean_float`] and
    /// emit as an `f64` JSON number so downstream consumers
    /// see `0.85` instead of `0.8500000238418579`.
    pub fn serialize<S>(val: &f32, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(clean_float(*val))
    }

    /// Default `f32` deserialiser — accepts both the clean Ryu
    /// form and the legacy `Display::fmt` form.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<f32, D::Error>
    where
        D: Deserializer<'de>,
    {
        f32::deserialize(deserializer)
    }
}

pub mod opt_scalar {
    //! `Option<f32>` (optional scalar with default `None`).
    use super::*;

    /// Serialise `Some(v)` through [`super::clean_float`] and
    /// `None` as a JSON `null`.
    pub fn serialize<S>(val: &Option<f32>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match val {
            Some(v) => serializer.serialize_some(&clean_float(*v)),
            None => serializer.serialize_none(),
        }
    }

    /// Default `Option<f32>` deserialiser — accepts both the
    /// clean Ryu form and the legacy `Display::fmt` form.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<f32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<f32>::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    /// Round-trip via `f32::to_bits()` — Ryu emits the shortest
    /// decimal that parses back to the same bits.
    fn round_trip_bits(v: f32) -> u32 {
        // `clean_float` returns `f64`; serde will serialise it
        // through whichever representation the consumer asked
        // for (JSON `Number`, TOML float). For the bit-identity
        // check we re-parse the string form Ryu would emit.
        let s = format!("{v}");
        let back: f32 = s.parse().expect("Ryu must reparse to f32");
        back.to_bits()
    }

    // 1. `serde_clean_f32_emits_shortest_round_trip_decimal`:
    //    `0.1`, `0.3`, `1.7` clean; the `Display::fmt` blobs do
    //    not appear.
    #[test]
    fn serde_clean_f32_emits_shortest_round_trip_decimal() {
        for v in [0.1_f32, 0.3, 1.7] {
            let s = format!("{v}");
            // `format!("{v}")` already uses Ryu for f32 since
            // Rust 1.55, so the helper is the same code path;
            // we also verify the form does NOT match the
            // pre-Ryu Display form.
            let display_form = match v {
                0.1 => "0.10000000149",
                0.3 => "0.30000001192",
                1.7 => "1.70000004768",
                _ => unreachable!(),
            };
            assert!(
                !s.starts_with(display_form),
                "expected Ryu-clean form for {v}, got {s:?}"
            );
        }

        // And end-to-end via the `vec` variant.
        #[derive(Serialize)]
        struct Wrapper {
            #[serde(with = "vec")]
            temps: Vec<f32>,
        }
        let w = Wrapper {
            temps: vec![0.1, 0.3, 1.7],
        };
        let j = serde_json::to_string(&w).expect("serialise");
        assert!(!j.contains("0.10000000149"), "got {j}");
        assert!(!j.contains("0.70000004768"), "got {j}");
        assert!(!j.contains("1.70000004768"), "got {j}");
        assert!(j.contains("0.1"), "got {j}");
        assert!(j.contains("0.3"), "got {j}");
        assert!(j.contains("1.7"), "got {j}");
    }

    // 2. `serde_clean_f32_preserves_operator_precision_up_to_ryu_limit`:
    //    round-trip idempotente por `to_bits()`.
    #[test]
    fn serde_clean_f32_preserves_operator_precision_up_to_ryu_limit() {
        let inputs = [0.1_f32, 0.75, 1.123, 1.1234, 1.7];
        for &v in &inputs {
            assert_eq!(
                round_trip_bits(v),
                v.to_bits(),
                "round-trip lost bits for {v:?}"
            );

            // End-to-end through serde_json + the `vec` variant.
            #[derive(Serialize, Deserialize)]
            struct Wrapper {
                #[serde(with = "vec")]
                temps: Vec<f32>,
            }
            let w = Wrapper { temps: vec![v] };
            let j = serde_json::to_string(&w).expect("serialise");
            let back: Wrapper = serde_json::from_str(&j).expect("deserialise");
            assert_eq!(
                back.temps[0].to_bits(),
                v.to_bits(),
                "lost bits for {v:?}: {j}"
            );
        }
    }

    // 3. `serde_clean_f32_does_not_touch_strings_outside_temperatures`:
    //    pin no-regresión: strings en un struct mixto (un campo
    //    `String` y un campo `f32` con helper) NO se ven
    //    afectados por el helper — el `f32` sí pasa por Ryu,
    //    el `String` queda verbatim. Pin crítico contra una
    //    refactor que accidentalmente aplique `clean_f32` a
    //    tipos no-f32.
    #[test]
    fn serde_clean_f32_does_not_touch_strings_outside_temperatures() {
        #[derive(Serialize, Deserialize, Debug)]
        struct Wrapper {
            #[serde(with = "scalar")]
            score: f32,
            timestamp: String,
            label: String,
        }
        let original = Wrapper {
            score: 0.7_f32,
            timestamp: "2026-08-26T20:21:56.123456Z".to_owned(),
            label: "custom-v2.1234".to_owned(),
        };
        let j = serde_json::to_string(&original).expect("serialise");
        // `f32` pasa por Ryu.
        assert!(
            j.contains("\"score\":0.7"),
            "score must be Ryu-clean, got {j}"
        );
        assert!(
            !j.contains("0.70000004768"),
            "score must NOT carry widening noise, got {j}"
        );
        // `String` queda verbatim (ni escapeado ni tocado).
        assert!(
            j.contains("\"timestamp\":\"2026-08-26T20:21:56.123456Z\""),
            "timestamp must be untouched, got {j}"
        );
        assert!(
            j.contains("\"label\":\"custom-v2.1234\""),
            "label must be untouched, got {j}"
        );
        // Round-trip bit-equal.
        let back: Wrapper = serde_json::from_str(&j).expect("deserialise");
        assert_eq!(back.score.to_bits(), original.score.to_bits());
        assert_eq!(back.timestamp, original.timestamp);
        assert_eq!(back.label, original.label);
    }

    // 4. `serde_clean_f32_handles_nan_and_infinity`: NaN, ±inf
    //    round-trip via `to_bits()`. **Pins the contract**: the
    //    helper itself never panics on NaN/inf (Ryu emits the
    //    parseable forms `NaN` / `inf` / `-inf`), BUT a field
    //    annotated with `#[serde(with = "crate::serde_util::
    //    clean_f32::scalar")]` whose value is NaN serialises
    //    to JSON `null` (because `serde_json::serialize_f64`
    //    rejects non-finite numbers per RFC 8259), and the
    //    subsequent deserialisation fails with
    //    `invalid type: null, expected f32`. This is the
    //    documented load-bearing behaviour — operators who
    //    encounter NaN in production must treat the sidecar
    //    as poisoned and re-probe from scratch.
    #[test]
    fn serde_clean_f32_handles_nan_and_infinity() {
        // Display strings — Rust's Ryu for f32 emits
        // `NaN` / `inf` / `-inf` (which `f32::parse` accepts).
        // We pin those as the load-bearing contract: any future
        // change to the Ryu output (e.g. switching to `Debug`
        // formatting, which emits `NaN` / `inf` too but with
        // different surrounding chars) surfaces here.
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let s = format!("{v}");
            assert!(s == "NaN" || s == "inf" || s == "-inf", "got {s:?}");
            let back: f32 = s.parse().expect("parse Ryu form");
            assert_eq!(
                back.to_bits(),
                v.to_bits(),
                "round-trip changed bits for {v:?}: {s}"
            );
        }

        // The `clean_float` helper converts to `f64` via
        // `f64::parse`. NaN / ±inf must NOT panic and must
        // round-trip as themselves.
        let nan_f64 = clean_float(f32::NAN);
        assert!(nan_f64.is_nan(), "NaN must round-trip as NaN: {nan_f64}");
        assert_eq!(clean_float(f32::INFINITY), f64::INFINITY);
        assert_eq!(clean_float(f32::NEG_INFINITY), f64::NEG_INFINITY);

        // And finite values still go through bit-identical.
        for v in [0.0_f32, -0.0, 1.5, f32::MIN_POSITIVE] {
            let s = format!("{v}");
            let back: f32 = s.parse().expect("parse Ryu form");
            assert_eq!(
                back.to_bits(),
                v.to_bits(),
                "finite round-trip changed bits for {v:?}: {s}"
            );
        }

        // **End-to-end via `serde_json`**: a `scalar`-annotated
        // field whose value is NaN serialises to JSON `null` and
        // the round-trip fails on deserialize. This is the
        // load-bearing behaviour the module doc-comment
        // promises. If a future `serde_json` release changes
        // this (e.g. emits `"NaN"` literally) the test will
        // flip and force a re-evaluation.
        #[derive(Serialize, Deserialize, Debug)]
        struct Holder {
            #[serde(with = "scalar")]
            v: f32,
        }
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let j = serde_json::to_string(&Holder { v }).expect("serialise");
            assert!(
                j.contains("null"),
                "serde_json serialises non-finite f32 as `null`; got {j}"
            );
            // Deserialisation back into `f32` MUST fail (this
            // is the guardrail against silent data corruption).
            let back: Result<Holder, _> = serde_json::from_str(&j);
            assert!(
                back.is_err(),
                "deserialising `null` as f32 must fail; back={back:?}"
            );
            let err = back.unwrap_err().to_string();
            assert!(
                err.contains("invalid type") || err.contains("null"),
                "expected an `invalid type` error mentioning `null`; got {err}"
            );
        }
    }

    // 5. `serde_clean_f32_scalar_variant`: RankEntry-like struct
    //    serialises as `"score": 0.85` and round-trips bit-equal.
    #[test]
    fn serde_clean_f32_scalar_variant() {
        #[derive(Serialize, Deserialize, Debug)]
        struct RankEntry {
            #[serde(with = "scalar")]
            score: f32,
        }
        let original = RankEntry { score: 0.85_f32 };
        let j = serde_json::to_string(&original).expect("serialise");
        assert!(j.contains("\"score\":0.85"), "expected clean form, got {j}");
        assert!(!j.contains("0.85000002384"), "got {j}");
        let back: RankEntry = serde_json::from_str(&j).expect("deserialise");
        assert_eq!(back.score.to_bits(), original.score.to_bits());
    }

    // 6. `serde_clean_f32_opt_scalar_variant`: Option<f32>
    //    serialises `Some(0.85)` as `0.85` and `None` as `null`.
    #[test]
    fn serde_clean_f32_opt_scalar_variant() {
        #[derive(Serialize, Deserialize, Debug)]
        struct Wrapper {
            #[serde(with = "opt_scalar", default)]
            val: Option<f32>,
        }
        // Some case.
        let original = Wrapper {
            val: Some(0.85_f32),
        };
        let j = serde_json::to_string(&original).expect("serialise");
        assert!(j.contains("\"val\":0.85"), "expected clean form, got {j}");
        let back: Wrapper = serde_json::from_str(&j).expect("deserialise");
        assert_eq!(back.val.map(|v| v.to_bits()), Some(0.85_f32.to_bits()));

        // None case.
        let original = Wrapper { val: None };
        let j = serde_json::to_string(&original).expect("serialise");
        assert!(j.contains("\"val\":null"), "expected null, got {j}");
        let back: Wrapper = serde_json::from_str(&j).expect("deserialise");
        assert!(back.val.is_none());
    }
}
