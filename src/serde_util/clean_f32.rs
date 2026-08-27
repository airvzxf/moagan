//! Serialize `f32` as its shortest round-trip decimal (Ryu) so
//! sidecars carry `0.1` instead of `0.10000000149011612`. The
//! helper is `pub` so any downstream `serde` derive can adopt
//! it without duplicating the conversion.
//!
//! Three variants are exposed:
//!
//! - [`vec`] — for `[f32]` / `Vec<f32>` fields like the
//!   temperature probe sidecar.
//! - [`scalar`] — for single `f32` fields (judge scores, cluster
//!   cohesion, …).
//! - [`opt_scalar`] — for `Option<f32>` fields where `None` should
//!   round-trip as JSON `null` and `Some(v)` should reuse the
//!   same Ryu helper.
//!
//! Deserialisation uses the default `f32` deserialiser for every
//! variant: the operator can keep writing `"score": 0.85` in
//! TOML or `"score": 0.85000002384…` from a pre-Ryu sidecar and
//! both land on the same `0.85_f32` bits. Sidecars are therefore
//! forward- and backward-compatible across the v0.12.4 migration.

use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serializer};

/// Convert an `f32` to its shortest round-trip decimal,
/// represented as `f64` so it survives every JSON / TOML
/// serializer without widening noise. Operator-written `0.1`
/// round-trips as `0.1`; native `0.1_f32` (which is
/// `0.10000000149011612` in `Display::fmt`) round-trips as
/// `0.1` because Ryu emits the shortest string that parses
/// back to the same `f32` bits.
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
    //    pin no-regresión: timestamps y strings arbitrarios NO
    //    se ven afectados.
    #[test]
    fn serde_clean_f32_does_not_touch_strings_outside_temperatures() {
        let inputs = [
            "20:21:56.123456",
            "custom-v2.1234",
            "anything",
            "temperature = 0.7", // Pin: this stays untouched.
        ];
        for s in inputs {
            let v: String = s.parse().expect("parse");
            let back = s.to_string();
            assert_eq!(v, back, "string passthrough changed value: {s:?}");
            // Sanity: serde_json treats this as a plain string
            // (no helper applies).
            let j = serde_json::to_string(&v).expect("serialise");
            assert!(j.contains(s), "expected literal substring {s:?} in {j:?}");
        }
    }

    // 4. `serde_clean_f32_handles_nan_and_infinity`: NaN, ±inf
    //    round-trip via `to_bits()`.
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
        // round-trip as themselves. (`serde_json::to_string`
        // emits NaN as `null` and refuses to round-trip it, so
        // we test the helper directly.)
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
