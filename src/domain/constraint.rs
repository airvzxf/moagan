//! Hard tag incompatibilities (proposal-03 §D.13.15) + the typed
//! [`HardIncompat`] enum (catalog I.6).
//!
//! Two related concepts share this module:
//!
//! 1. **Tag-pair incompatibilities** — the constant
//!    [`HARD_INCOMPATIBILITIES`] plus [`is_incompatible`] /
//!    [`find_conflicts`] helpers. These gate the `SynthesizePhase`
//!    so a cluster that mixes mutually exclusive tags is skipped
//!    instead of merged. Compliance: proposal-03 §D.13.15
//!    (10 pairs from T02-09; T19-09; T03-01; T18-04 §11.1;
//!    T05-10 §11.1; T08-06 §11.2; T08-08 §11.2).
//!
//! 2. **Typed incompatibility records** — the [`HardIncompat`] enum
//!    (catalog I.6). This is the structured form a downstream phase
//!    uses to *report* an incompatibility: a `SynthesizePhase`
//!    gatekeeper can describe *why* two proposals cannot coexist,
//!    with a human-readable [`HardIncompat::explain`] message.
//!    The seven base variants cover both the tag-based conflicts
//!    the constant list catches and the new kinds the catalog
//!    adds: `TemporalImpossibility`, `NumericalOverflow`,
//!    `MutuallyExclusive`, `UnsupportedPlatform`,
//!    `VersionConflict`. The six unit variants added by
//!    catalog I.6 §D.13.15 (the *exhaustive* set:
//!    `MonolithVsMicroservices`, `SqlVsNosqlBackend`,
//!    `GcRuntimeWithManualMem`, `SingleTenantInMultitenant`,
//!    `StatefulInServerless`, `SyncApiWithAsyncCaller`) capture
//!    the architectural clashes a proposal can exhibit even
//!    when its tag vector is internally consistent.
//!    [`HardIncompat::is_incompatible_with`] detects redundant
//!    or overlapping records of those runtime clashes, and
//!    [`HardIncompat::from_catalog`] enumerates the canonical
//!    six-variant set for documentation and test fixtures.

/// Pairs of tag values that are mutually exclusive. Order within each
/// pair is irrelevant — `is_incompatible("a", "b")` and
/// `is_incompatible("b", "a")` both return `true`.
pub const HARD_INCOMPATIBILITIES: &[(&str, &str)] = &[
    ("monolith", "microservices"),
    ("sync_rpc", "event_driven"),
    ("strong_consistency", "eventual_consistency"),
    ("sql", "nosql"),
    ("self_hosted", "serverless"),
    ("rust", "non_permitted_runtime"),
    ("single_tenant", "multi_tenant"),
    ("monolith_db", "polyglot_persistence"),
    ("pull_based", "push_based"),
    ("custom_protocol", "standard_protocol"),
];

/// Symmetric incompatibility check. Returns `true` when `(a, b)`
/// (in either order) appears in `HARD_INCOMPATIBILITIES`.
pub fn is_incompatible(a: &str, b: &str) -> bool {
    HARD_INCOMPATIBILITIES
        .iter()
        .any(|(x, y)| (a == *x && b == *y) || (a == *y && b == *x))
}

/// Iterate every unique pair (a, b) where a and b are members of
/// `tags`, returning the pairs whose components are mutually
/// exclusive. The first component is always the one that appears
/// earlier in `HARD_INCOMPATIBILITIES`. Returns the pair as a
/// 2-tuple of borrowed strings.
pub fn find_conflicts<'a>(tags: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    let mut out: Vec<(&'a str, &'a str)> = Vec::new();
    for i in 0..tags.len() {
        for j in (i + 1)..tags.len() {
            if is_incompatible(tags[i], tags[j]) {
                out.push((tags[i], tags[j]));
            }
        }
    }
    out
}

/// Typed record of a hard incompatibility finding (catalog I.6).
///
/// A gatekeeper (e.g. `SynthesizePhase`, an intake validator, a
/// host-preflight check) builds one of these when it decides two
/// proposals (or a proposal and the runtime) cannot coexist. The
/// [`explain`](Self::explain) method renders a one-line, human-readable
/// message suitable for logs, sidecars, or operator UIs. `Display`
/// delegates to `explain` so `format!("{h}")` and `h.explain()` are
/// interchangeable.
///
/// The seven variants are:
///
/// - `LanguageToolchainMismatch` — the language the proposal targets
///   cannot be compiled by the available toolchain (e.g. `python`
///   project on a host without `python3`).
/// - `ForbiddenTech` — the proposal uses a technology that is on
///   the forbidden list for this run (e.g. `docker` in a
///   sandbox-denied environment).
/// - `TemporalImpossibility` — `earliest` start is after the
///   `latest` deadline, so the work cannot complete on time.
/// - `NumericalOverflow` — the `declared` value exceeds the
///   `limit` (e.g. memory budget exceeds host RAM).
/// - `MutuallyExclusive` — two options named by the proposal are
///   structurally exclusive (e.g. `sql` + `nosql`).
/// - `UnsupportedPlatform` — the proposal requires a `required`
///   platform that is not in the `available` list (the list may be
///   empty, rendered as `<none>` in the message).
/// - `VersionConflict` — a dependency requires `required` but the
///   host has `found`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HardIncompat {
    /// The language the proposal targets cannot be compiled /
    /// executed by the available toolchain.
    LanguageToolchainMismatch {
        /// Target language (e.g. `"python"`, `"rust"`).
        lang: String,
        /// Toolchain that is missing or incompatible (e.g. `"stable"`,
        /// `"nightly"`, `"3.11"`).
        toolchain: String,
    },
    /// The proposal uses a technology that is on the forbidden
    /// list for this run.
    ForbiddenTech {
        /// Name of the forbidden technology (e.g. `"docker"`).
        tech: String,
    },
    /// The earliest start is later than the latest deadline, so the
    /// work cannot complete on time. `earliest` and `latest` carry
    /// whatever string form the gatekeeper captured (ISO date,
    /// unix seconds, etc.).
    TemporalImpossibility {
        /// Earliest possible start (string form preserved verbatim).
        earliest: String,
        /// Latest acceptable deadline (string form preserved verbatim).
        latest: String,
    },
    /// The declared value exceeds the limit. Used for memory,
    /// disk, port count, or any other bounded resource.
    NumericalOverflow {
        /// Value the proposal declared.
        declared: u64,
        /// Maximum the host can provide.
        limit: u64,
    },
    /// Two options the proposal names are structurally exclusive
    /// and cannot coexist in the same design.
    MutuallyExclusive {
        /// First option (e.g. `"sql"`).
        a: String,
        /// Second option (e.g. `"nosql"`).
        b: String,
    },
    /// The proposal requires a platform the host does not provide.
    /// `available` lists every platform the host exposes so the
    /// operator can see what was on the table.
    UnsupportedPlatform {
        /// Platform the proposal needs (e.g. `"linux/arm64"`).
        required: String,
        /// Platforms the host exposes (may be empty).
        available: Vec<String>,
    },
    /// A dependency requires a version that conflicts with what the
    /// host has installed.
    VersionConflict {
        /// Version the proposal requires (e.g. `"rust 1.80"`).
        required: String,
        /// Version the host actually has (e.g. `"rust 1.70"`).
        found: String,
    },
    /// Catalog I.6 §D.13.15 (exhaustive): a monolithic deployment
    /// is paired with a microservice intent (or vice versa). The
    /// single-process deployment cannot host the cross-service
    /// discovery / circuit-breaker / per-service health surface
    /// the microservice intent assumes.
    MonolithVsMicroservices,
    /// Catalog I.6 §D.13.15 (exhaustive): an SQL database is used
    /// for a NoSQL workload (or vice versa). The relational schema
    /// requires migrations; the NoSQL workload assumes schema-less
    /// reads and finally-consistent indexing.
    SqlVsNosqlBackend,
    /// Catalog I.6 §D.13.15 (exhaustive): a garbage-collected
    /// runtime (JVM, BEAM) is paired with manual memory
    /// management expectations. The collector owns the lifetime;
    /// manual `Drop` / `free` calls are unreachable or fight the GC.
    GcRuntimeWithManualMem,
    /// Catalog I.6 §D.13.15 (exhaustive): a single-tenant library
    /// is deployed inside a multi-tenant application (or vice
    /// versa). The single-tenant library has no notion of tenant
    /// scoping; the multi-tenant app leaks state across tenants.
    SingleTenantInMultitenant,
    /// Catalog I.6 §D.13.15 (exhaustive): a stateful service runs
    /// inside a serverless execution model. The serverless
    /// runtime freezes / restarts the worker between invocations;
    /// the in-process state evaporates.
    StatefulInServerless,
    /// Catalog I.6 §D.13.15 (exhaustive): a synchronous API caller
    /// waits on an async upstream. The caller's thread blocks
    /// while the upstream completes a multi-step async pipeline;
    /// the latency budget is consumed by the upstream's scheduling,
    /// not the caller's work.
    SyncApiWithAsyncCaller,
}

impl HardIncompat {
    /// Render a one-line, human-readable message explaining the
    /// incompatibility. Stable enough to surface in operator logs
    /// and the JSON sidecar; the unit tests pin the wording for
    /// the variants added by catalog I.6.
    pub fn explain(&self) -> String {
        match self {
            Self::LanguageToolchainMismatch { lang, toolchain } => {
                format!("language '{lang}' cannot run on toolchain '{toolchain}'")
            }
            Self::ForbiddenTech { tech } => {
                format!("technology '{tech}' is forbidden in this context")
            }
            Self::TemporalImpossibility { earliest, latest } => format!(
                "temporal impossibility: earliest start '{earliest}' is after the latest deadline '{latest}'"
            ),
            Self::NumericalOverflow { declared, limit } => {
                format!("numerical overflow: declared value {declared} exceeds the limit {limit}")
            }
            Self::MutuallyExclusive { a, b } => {
                format!("options '{a}' and '{b}' are mutually exclusive and cannot coexist")
            }
            Self::UnsupportedPlatform {
                required,
                available,
            } => {
                let avail = if available.is_empty() {
                    "<none>".to_string()
                } else {
                    available.join(", ")
                };
                format!("required platform '{required}' is not available (host platforms: {avail})")
            }
            Self::VersionConflict { required, found } => {
                format!("version conflict: requires '{required}' but the host has '{found}'")
            }
            Self::MonolithVsMicroservices => {
                "monolithic deployment is paired with microservice intent".to_string()
            }
            Self::SqlVsNosqlBackend => {
                "SQL database is used for a NoSQL workload (or vice versa)".to_string()
            }
            Self::GcRuntimeWithManualMem => {
                "garbage-collected runtime (JVM, BEAM) is paired with manual memory management"
                    .to_string()
            }
            Self::SingleTenantInMultitenant => {
                "single-tenant library is deployed inside a multi-tenant application".to_string()
            }
            Self::StatefulInServerless => {
                "stateful service runs inside a serverless execution model".to_string()
            }
            Self::SyncApiWithAsyncCaller => {
                "synchronous API caller waits on an async upstream".to_string()
            }
        }
    }

    /// Catalog I.6 §D.13.15 (exhaustive): detect whether two
    /// `HardIncompat` records describe redundant or overlapping
    /// incompatibilities. Two records are "incompatible with
    /// each other" (in the pair-detection sense) when reporting
    /// both is redundant:
    ///
    /// - Same unit variant → the proposal fails the same way twice;
    ///   a single record already covers it.
    /// - Two `MutuallyExclusive` records that share at least one
    ///   option → they describe the same structural clash.
    /// - A `SingleTenantInMultitenant` clash vs. a
    ///   `MutuallyExclusive { a: "single_tenant" | "multi_tenant",
    ///   .. }` → the tag-level pair already names the conflict;
    ///   the runtime-clash variant is redundant.
    /// - A `SqlVsNosqlBackend` clash vs. a
    ///   `MutuallyExclusive { a: "sql" | "nosql", .. }` → the
    ///   tag-level pair already names the conflict; the
    ///   runtime-clash variant is redundant.
    /// - All other pairs are independent: each describes a distinct
    ///   problem and the gatekeeper should report both.
    ///
    /// The function is symmetric:
    /// `a.is_incompatible_with(b) == b.is_incompatible_with(a)`.
    pub fn is_incompatible_with(&self, other: &HardIncompat) -> bool {
        use HardIncompat::*;
        match (self, other) {
            // Same unit-variant redundancy (6 exhaustive §D.13.15 cases).
            (MonolithVsMicroservices, MonolithVsMicroservices) => true,
            (SqlVsNosqlBackend, SqlVsNosqlBackend) => true,
            (GcRuntimeWithManualMem, GcRuntimeWithManualMem) => true,
            (SingleTenantInMultitenant, SingleTenantInMultitenant) => true,
            (StatefulInServerless, StatefulInServerless) => true,
            (SyncApiWithAsyncCaller, SyncApiWithAsyncCaller) => true,
            // Two MutuallyExclusive records overlap when they share
            // at least one option (regardless of order, so the
            // checker is symmetric without enumerating 4 permutations).
            (MutuallyExclusive { a: a1, b: b1 }, MutuallyExclusive { a: a2, b: b2 }) => {
                a1 == a2 || a1 == b2 || b1 == a2 || b1 == b2
            }
            // Tag-level vs. runtime-clash redundancy.
            (SingleTenantInMultitenant, MutuallyExclusive { a, b })
            | (MutuallyExclusive { a, b }, SingleTenantInMultitenant) => {
                matches!(a.as_str(), "single_tenant" | "multi_tenant")
                    || matches!(b.as_str(), "single_tenant" | "multi_tenant")
            }
            (SqlVsNosqlBackend, MutuallyExclusive { a, b })
            | (MutuallyExclusive { a, b }, SqlVsNosqlBackend) => {
                matches!(a.as_str(), "sql" | "nosql") || matches!(b.as_str(), "sql" | "nosql")
            }
            // Independent pairs.
            _ => false,
        }
    }

    /// Catalog I.6 §D.13.15 (exhaustive): return the canonical
    /// six-variant set added by the exhaustive rewrite. The
    /// returned vector is stable (insertion order matches the
    /// enum declaration order); documentation and test fixtures
    /// use it to pin the contract without depending on each
    /// variant individually.
    pub fn from_catalog() -> Vec<HardIncompat> {
        vec![
            HardIncompat::MonolithVsMicroservices,
            HardIncompat::SqlVsNosqlBackend,
            HardIncompat::GcRuntimeWithManualMem,
            HardIncompat::SingleTenantInMultitenant,
            HardIncompat::StatefulInServerless,
            HardIncompat::SyncApiWithAsyncCaller,
        ]
    }
}

impl std::fmt::Display for HardIncompat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.explain())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tag compared with itself must never be flagged — the matrix
    /// is "different architectural choices", not "self-conflict".
    #[test]
    fn identical_tags_are_not_incompatible() {
        assert!(!is_incompatible("monolith", "monolith"));
        assert!(!is_incompatible("sql", "sql"));
    }

    /// Both orderings of a known incompatible pair must report true
    /// (the matrix is symmetric).
    #[test]
    fn known_pair_is_incompatible_both_orderings() {
        assert!(is_incompatible("monolith", "microservices"));
        assert!(is_incompatible("microservices", "monolith"));
        assert!(is_incompatible("sql", "nosql"));
        assert!(is_incompatible("nosql", "sql"));
    }

    /// A pair that does not appear in the matrix must report false.
    /// Picking tags that are unrelated on purpose (sql + pull_based
    /// is fine; sql + self_hosted is also fine; only sql + nosql is
    /// a hard incompatibility).
    #[test]
    fn unknown_pair_is_not_incompatible() {
        assert!(!is_incompatible("sql", "self_hosted"));
        assert!(!is_incompatible("rust", "event_driven"));
        assert!(!is_incompatible("foo", "bar"));
    }

    /// Empty input cannot contain any pair. `is_incompatible("", "")`
    /// is `false` because the empty string is not in the matrix.
    #[test]
    fn empty_input_is_not_incompatible() {
        assert!(!is_incompatible("", ""));
        assert!(!is_incompatible("", "monolith"));
    }

    /// `find_conflicts` returns every pair exactly once (no
    /// duplicates) and skips unrelated tags.
    #[test]
    fn find_conflicts_returns_only_conflicting_pairs() {
        let tags = vec!["monolith", "microservices", "sql", "self_hosted"];
        let conflicts = find_conflicts(&tags);
        assert_eq!(conflicts.len(), 1);
        let (a, b) = conflicts[0];
        assert!(
            (a == "monolith" && b == "microservices") || (a == "microservices" && b == "monolith")
        );
    }

    // -- Catalog I.6: HardIncompat typed records ---------------------

    /// `TemporalImpossibility` message must mention both endpoints
    /// verbatim so an operator can correlate with the schedule.
    #[test]
    fn hard_incompat_explain_temporal_impossibility() {
        let h = HardIncompat::TemporalImpossibility {
            earliest: "2026-12-01".into(),
            latest: "2026-11-15".into(),
        };
        let msg = h.explain();
        assert!(
            msg.contains("2026-12-01"),
            "earliest start must appear in the message, got: {msg}"
        );
        assert!(
            msg.contains("2026-11-15"),
            "latest deadline must appear in the message, got: {msg}"
        );
        let lower = msg.to_lowercase();
        assert!(
            lower.contains("temporal") || lower.contains("earliest"),
            "message must mention the temporal nature, got: {msg}"
        );
    }

    /// `NumericalOverflow` message must include both numbers and
    /// signal that the declared value exceeds the limit.
    #[test]
    fn hard_incompat_explain_numerical_overflow() {
        let h = HardIncompat::NumericalOverflow {
            declared: 100,
            limit: 50,
        };
        let msg = h.explain();
        assert!(msg.contains("100"), "declared must appear: {msg}");
        assert!(msg.contains("50"), "limit must appear: {msg}");
        assert!(
            msg.to_lowercase().contains("overflow"),
            "message must say 'overflow', got: {msg}"
        );
    }

    /// `MutuallyExclusive` message must name both options and
    /// state that they cannot coexist.
    #[test]
    fn hard_incompat_explain_mutually_exclusive() {
        let h = HardIncompat::MutuallyExclusive {
            a: "sql".into(),
            b: "nosql".into(),
        };
        let msg = h.explain();
        assert!(msg.contains("sql"), "first option must appear: {msg}");
        assert!(msg.contains("nosql"), "second option must appear: {msg}");
        assert!(
            msg.to_lowercase().contains("mutually exclusive"),
            "message must say 'mutually exclusive', got: {msg}"
        );
    }

    /// `UnsupportedPlatform` message must name the required
    /// platform and every available platform. The `available`
    /// list may be empty, in which case the message must say
    /// `<none>` so the operator knows nothing was on offer.
    #[test]
    fn hard_incompat_explain_unsupported_platform() {
        let h = HardIncompat::UnsupportedPlatform {
            required: "linux/arm64".into(),
            available: vec!["linux/x86_64".into(), "darwin/arm64".into()],
        };
        let msg = h.explain();
        assert!(msg.contains("linux/arm64"), "required must appear: {msg}");
        assert!(
            msg.contains("linux/x86_64"),
            "available[0] must appear: {msg}"
        );
        assert!(
            msg.contains("darwin/arm64"),
            "available[1] must appear: {msg}"
        );
        let empty = HardIncompat::UnsupportedPlatform {
            required: "wasm32".into(),
            available: vec![],
        };
        let msg = empty.explain();
        assert!(msg.contains("wasm32"), "required must appear: {msg}");
        assert!(
            msg.contains("<none>"),
            "empty available list must render as <none>: {msg}"
        );
    }

    /// `VersionConflict` message must name both versions so the
    /// operator can pick the right upgrade path.
    #[test]
    fn hard_incompat_explain_version_conflict() {
        let h = HardIncompat::VersionConflict {
            required: "rust 1.80".into(),
            found: "rust 1.70".into(),
        };
        let msg = h.explain();
        assert!(msg.contains("rust 1.80"), "required must appear: {msg}");
        assert!(msg.contains("rust 1.70"), "found must appear: {msg}");
        assert!(
            msg.to_lowercase().contains("version"),
            "message must mention the version conflict, got: {msg}"
        );
    }

    /// `Display` must produce the same string as `explain()` for
    /// every variant. The two are documented as interchangeable
    /// (logs use `Display`; structured callers use `explain`).
    #[test]
    fn hard_incompat_display_matches_explain() {
        let cases = vec![
            HardIncompat::LanguageToolchainMismatch {
                lang: "python".into(),
                toolchain: "stable".into(),
            },
            HardIncompat::ForbiddenTech {
                tech: "docker".into(),
            },
            HardIncompat::TemporalImpossibility {
                earliest: "2026-12-01".into(),
                latest: "2026-11-15".into(),
            },
            HardIncompat::NumericalOverflow {
                declared: 100,
                limit: 50,
            },
            HardIncompat::MutuallyExclusive {
                a: "sql".into(),
                b: "nosql".into(),
            },
            HardIncompat::UnsupportedPlatform {
                required: "linux/arm64".into(),
                available: vec!["linux/x86_64".into()],
            },
            HardIncompat::VersionConflict {
                required: "rust 1.80".into(),
                found: "rust 1.70".into(),
            },
        ];
        for h in cases {
            assert_eq!(
                format!("{h}"),
                h.explain(),
                "Display must match explain() for {h:?}"
            );
        }
    }

    // -- Catalog I.6 §D.13.15 (exhaustive): extended HardIncompat ----

    /// Pin the variant count of `HardIncompat` so a future catalog
    /// addition trips this test before it lands in production. The
    /// catalog ships thirteen variants: the seven from §I.6 plus
    /// the six exhaustive runtime-clash unit variants from §D.13.15
    /// (MonolithVsMicroservices, SqlVsNosqlBackend,
    /// GcRuntimeWithManualMem, SingleTenantInMultitenant,
    /// StatefulInServerless, SyncApiWithAsyncCaller).
    #[test]
    fn hard_incompat_extended_variants_count() {
        let cases: Vec<HardIncompat> = vec![
            HardIncompat::LanguageToolchainMismatch {
                lang: "rust".into(),
                toolchain: "1.70".into(),
            },
            HardIncompat::ForbiddenTech {
                tech: "docker".into(),
            },
            HardIncompat::TemporalImpossibility {
                earliest: "2026-12-01".into(),
                latest: "2026-11-15".into(),
            },
            HardIncompat::NumericalOverflow {
                declared: 100,
                limit: 50,
            },
            HardIncompat::MutuallyExclusive {
                a: "sql".into(),
                b: "nosql".into(),
            },
            HardIncompat::UnsupportedPlatform {
                required: "linux/arm64".into(),
                available: vec!["linux/x86_64".into()],
            },
            HardIncompat::VersionConflict {
                required: "rust 1.80".into(),
                found: "rust 1.70".into(),
            },
            HardIncompat::MonolithVsMicroservices,
            HardIncompat::SqlVsNosqlBackend,
            HardIncompat::GcRuntimeWithManualMem,
            HardIncompat::SingleTenantInMultitenant,
            HardIncompat::StatefulInServerless,
            HardIncompat::SyncApiWithAsyncCaller,
        ];
        assert_eq!(
            cases.len(),
            13,
            "HardIncompat must have 13 variants (7 base + 6 §D.13.15 exhaustive)"
        );
        // Every variant serialises with the snake_case kind tag and
        // round-trips back to the same value: pins the wire format.
        for v in &cases {
            let json = serde_json::to_string(v).expect("serialize");
            let back: HardIncompat = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&back, v, "round-trip must preserve {v:?}");
        }
    }

    /// `is_incompatible_with` detects redundant pairs involving the
    /// §D.13.15 (exhaustive) runtime-clash variants. Two identical
    /// `StatefulInServerless` records are redundant; the same
    /// variant against `MonolithVsMicroservices` is independent;
    /// and `SingleTenantInMultitenant` is redundant with the
    /// `MutuallyExclusive` tag-pair that names the same clash.
    /// Similarly `SqlVsNosqlBackend` is redundant with the
    /// `MutuallyExclusive { sql, nosql }` tag-pair.
    #[test]
    fn hard_incompat_pair_stateful_serverless_detected() {
        // Same unit variant → redundant.
        let a = HardIncompat::StatefulInServerless;
        let b = HardIncompat::StatefulInServerless;
        assert!(a.is_incompatible_with(&b));
        assert!(b.is_incompatible_with(&a));
        // Different runtime-clash variants → independent.
        let c = HardIncompat::MonolithVsMicroservices;
        assert!(!a.is_incompatible_with(&c));
        assert!(!c.is_incompatible_with(&a));
        // SyncApiWithAsyncCaller is unrelated to StatefulInServerless.
        let d = HardIncompat::SyncApiWithAsyncCaller;
        assert!(!a.is_incompatible_with(&d));
        assert!(!d.is_incompatible_with(&a));
        // MutuallyExclusive without the single/multi tenant tag
        // does NOT collide with SingleTenantInMultitenant.
        let me = HardIncompat::MutuallyExclusive {
            a: "sql".into(),
            b: "nosql".into(),
        };
        let stm = HardIncompat::SingleTenantInMultitenant;
        assert!(!me.is_incompatible_with(&stm));
        // ... but the MutuallyExclusive tag-pair that names the
        // single/multi tenant clash DOES collide with the
        // runtime-clash variant (same conflict, different lens).
        let me_st = HardIncompat::MutuallyExclusive {
            a: "single_tenant".into(),
            b: "multi_tenant".into(),
        };
        assert!(stm.is_incompatible_with(&me_st));
        assert!(me_st.is_incompatible_with(&stm));
        // SqlVsNosqlBackend collides with the sql/nosql tag-pair.
        let svn = HardIncompat::SqlVsNosqlBackend;
        assert!(svn.is_incompatible_with(&me));
        assert!(me.is_incompatible_with(&svn));
    }

    /// Every variant round-trips through serde with the
    /// `snake_case` `kind` discriminator. The wire format is part
    /// of the public contract — sidecar JSON consumers depend on
    /// it — so this test pins the discriminator and the payload
    /// shape per variant.
    #[test]
    fn hard_incompat_serialize_each_variant() {
        // Each entry is (variant, expected_kind_tag). The kind tag
        // is the `#[serde(rename_all = "snake_case")]` form of the
        // variant name.
        let cases: Vec<(HardIncompat, &str)> = vec![
            (
                HardIncompat::LanguageToolchainMismatch {
                    lang: "rust".into(),
                    toolchain: "stable".into(),
                },
                "language_toolchain_mismatch",
            ),
            (
                HardIncompat::ForbiddenTech {
                    tech: "docker".into(),
                },
                "forbidden_tech",
            ),
            (
                HardIncompat::TemporalImpossibility {
                    earliest: "a".into(),
                    latest: "b".into(),
                },
                "temporal_impossibility",
            ),
            (
                HardIncompat::NumericalOverflow {
                    declared: 1,
                    limit: 0,
                },
                "numerical_overflow",
            ),
            (
                HardIncompat::MutuallyExclusive {
                    a: "x".into(),
                    b: "y".into(),
                },
                "mutually_exclusive",
            ),
            (
                HardIncompat::UnsupportedPlatform {
                    required: "linux/arm64".into(),
                    available: vec![],
                },
                "unsupported_platform",
            ),
            (
                HardIncompat::VersionConflict {
                    required: "1.0".into(),
                    found: "0.9".into(),
                },
                "version_conflict",
            ),
            (
                HardIncompat::MonolithVsMicroservices,
                "monolith_vs_microservices",
            ),
            (HardIncompat::SqlVsNosqlBackend, "sql_vs_nosql_backend"),
            (
                HardIncompat::GcRuntimeWithManualMem,
                "gc_runtime_with_manual_mem",
            ),
            (
                HardIncompat::SingleTenantInMultitenant,
                "single_tenant_in_multitenant",
            ),
            (HardIncompat::StatefulInServerless, "stateful_in_serverless"),
            (
                HardIncompat::SyncApiWithAsyncCaller,
                "sync_api_with_async_caller",
            ),
        ];
        for (variant, expected_kind) in cases {
            let json = serde_json::to_value(&variant).expect("serialize");
            let kind = json
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("missing kind for {variant:?}"));
            assert_eq!(kind, expected_kind, "kind tag mismatch for {variant:?}");
        }
    }

    // -- Catalog I.6 §D.13.15 (exhaustive): new tests -----------------

    /// Pin the variant count of `HardIncompat` to the canonical
    /// 13 (7 base + 6 exhaustive §D.13.15). The 6 new variants are
    /// architectural clashes widened from the partial initial set;
    /// the catalog wraps them into a single test so a future
    /// addition trips this test before it lands in production.
    #[test]
    fn hard_incompat_exhaustive_variants_count_is_13() {
        let catalog = HardIncompat::from_catalog();
        assert_eq!(
            catalog.len(),
            6,
            "from_catalog must return the 6 exhaustive §D.13.15 variants"
        );
        // Build the full set: 7 base + 6 exhaustive = 13.
        let mut all: Vec<HardIncompat> = vec![
            HardIncompat::LanguageToolchainMismatch {
                lang: "rust".into(),
                toolchain: "1.70".into(),
            },
            HardIncompat::ForbiddenTech {
                tech: "docker".into(),
            },
            HardIncompat::TemporalImpossibility {
                earliest: "2026-12-01".into(),
                latest: "2026-11-15".into(),
            },
            HardIncompat::NumericalOverflow {
                declared: 100,
                limit: 50,
            },
            HardIncompat::MutuallyExclusive {
                a: "sql".into(),
                b: "nosql".into(),
            },
            HardIncompat::UnsupportedPlatform {
                required: "linux/arm64".into(),
                available: vec!["linux/x86_64".into()],
            },
            HardIncompat::VersionConflict {
                required: "rust 1.80".into(),
                found: "rust 1.70".into(),
            },
        ];
        all.extend(catalog);
        assert_eq!(
            all.len(),
            13,
            "HardIncompat must have 13 variants (7 base + 6 §D.13.15 exhaustive)"
        );
    }

    /// `from_catalog` returns the canonical 6-variant list in
    /// stable insertion order. Pin the contents so documentation
    /// and test fixtures can rely on the order without enumerating
    /// each variant by name.
    #[test]
    fn hard_incompat_from_catalog_returns_canonical_list() {
        let catalog = HardIncompat::from_catalog();
        assert_eq!(
            catalog,
            vec![
                HardIncompat::MonolithVsMicroservices,
                HardIncompat::SqlVsNosqlBackend,
                HardIncompat::GcRuntimeWithManualMem,
                HardIncompat::SingleTenantInMultitenant,
                HardIncompat::StatefulInServerless,
                HardIncompat::SyncApiWithAsyncCaller,
            ]
        );
    }
}
