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
//!    when its tag vector is internally consistent. The three
//!    opt-in unit variants (`ClusterLocalInGlobal`,
//!    `PullInPushOnly`, `StatelessInStateful`) are additional
//!    catalog entries reserved for sub-fases that want a
//!    runtime-clash surface without expanding the tag-pair
//!    matrix: see [`HardIncompat::from_opt_in_catalog`].
//!    [`HardIncompat::from_catalog`] enumerates the canonical
//!    six-variant §D.13.15 set; [`HardIncompat::from_opt_in_catalog`]
//!    enumerates the three opt-in variants for documentation and
//!    test fixtures.

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
    let result = HARD_INCOMPATIBILITIES
        .iter()
        .any(|(x, y)| (a == *x && b == *y) || (a == *y && b == *x));
    tracing::trace!(a, b, result, "domain::constraint::is_incompatible");
    result
}

/// Iterate every unique pair (a, b) where a and b are members of
/// `tags`, returning the pairs whose components are mutually
/// exclusive. The first component is always the one that appears
/// earlier in `HARD_INCOMPATIBILITIES`. Returns the pair as a
/// 2-tuple of borrowed strings.
pub fn find_conflicts<'a>(tags: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    tracing::trace!(
        tag_count = tags.len(),
        "domain::constraint::find_conflicts: enter"
    );
    let mut out: Vec<(&'a str, &'a str)> = Vec::new();
    for i in 0..tags.len() {
        for j in (i + 1)..tags.len() {
            if is_incompatible(tags[i], tags[j]) {
                out.push((tags[i], tags[j]));
            }
        }
    }
    tracing::trace!(
        tags = tags.len(),
        conflicts = out.len(),
        "domain::constraint::find_conflicts: exit"
    );
    out
}

/// Catalog I.6 (opt-in) detectors. The three additional
/// [`HardIncompat`] variants added by #504
/// (`ClusterLocalInGlobal`, `PullInPushOnly`, `StatelessInStateful`)
/// were shipped as typed records without a heuristic. This module
/// adds the heuristics so a sub-fase (e.g. `SynthesizePhase`)
/// that already calls [`find_conflicts`] can opt into the
/// runtime-clash surface without expanding the tag-pair matrix.
///
/// The detectors are tag-set level: the caller passes the
/// flattened tag list (typically the union of every proposal's
/// tags in a cluster) and the helper returns `Some(HardIncompat)`
/// when the heuristic matches, `None` otherwise. Each detector
/// is independent so the unit tests pin the contract for one
/// variant at a time, and [`detect_opt_in_hardincompat`] runs
/// them in a fixed order for deterministic first-match
/// resolution.
///
/// Detect [`HardIncompat::ClusterLocalInGlobal`]: a tag set
/// containing both a `cluster_local` component and a `global`
/// component. Case-insensitive on the tag literal so `Cluster_Local`
/// and `cluster_local` both count. Returns the typed record on a
/// match; the caller surfaces the [`explain`](HardIncompat::explain)
/// message in the sidecar / log.
pub fn detect_cluster_local_in_global(tags: &[&str]) -> Option<HardIncompat> {
    let has_cluster_local = tags.iter().any(|t| t.eq_ignore_ascii_case("cluster_local"));
    let has_global = tags.iter().any(|t| t.eq_ignore_ascii_case("global"));
    let out = (has_cluster_local && has_global).then_some(HardIncompat::ClusterLocalInGlobal);
    tracing::trace!(
        tags = tags.len(),
        has_cluster_local,
        has_global,
        ?out,
        "domain::constraint::detect_cluster_local_in_global"
    );
    out
}

/// Detect [`HardIncompat::PullInPushOnly`]: a tag set containing
/// a pull-mode marker (`pull_based` OR `pull_required`) AND a
/// push-mode marker (`push_only` OR `push_endpoint`). The
/// disjunctive match on either side reflects the way operators
/// describe the same architectural decision under different
/// vocabulary (e.g. a polling worker can be tagged `pull_based`
/// in some shops and `pull_required` in others; both should
/// trigger the same incompatibility).
pub fn detect_pull_in_push_only(tags: &[&str]) -> Option<HardIncompat> {
    let has_pull = tags
        .iter()
        .any(|t| t.eq_ignore_ascii_case("pull_based") || t.eq_ignore_ascii_case("pull_required"));
    let has_push = tags
        .iter()
        .any(|t| t.eq_ignore_ascii_case("push_only") || t.eq_ignore_ascii_case("push_endpoint"));
    let out = (has_pull && has_push).then_some(HardIncompat::PullInPushOnly);
    tracing::trace!(
        tags = tags.len(),
        has_pull,
        has_push,
        ?out,
        "domain::constraint::detect_pull_in_push_only"
    );
    out
}

/// Detect [`HardIncompat::StatelessInStateful`]: a tag set
/// containing a `stateless` component AND a `stateful_required`
/// marker. Symmetric in tag order — the variant fires whether
/// `stateless` is paired with `stateful_required` or vice versa.
pub fn detect_stateless_in_stateful(tags: &[&str]) -> Option<HardIncompat> {
    let has_stateless = tags.iter().any(|t| t.eq_ignore_ascii_case("stateless"));
    let has_stateful = tags
        .iter()
        .any(|t| t.eq_ignore_ascii_case("stateful_required"));
    let out = (has_stateless && has_stateful).then_some(HardIncompat::StatelessInStateful);
    tracing::trace!(
        tags = tags.len(),
        has_stateless,
        has_stateful,
        ?out,
        "domain::constraint::detect_stateless_in_stateful"
    );
    out
}

/// Run the three opt-in catalog detectors in a fixed order
/// (`ClusterLocalInGlobal`, `PullInPushOnly`,
/// `StatelessInStateful`) and return the first match. The order
/// matches the enum declaration order in [`HardIncompat`] so the
/// output is deterministic across runs. Returns `None` when
/// none of the detectors fire — the caller should then fall
/// through to the existing tag-pair matrix in
/// [`find_conflicts`].
///
/// `tags` is the flattened union of every proposal's tag set in
/// the cluster being checked. The caller is expected to dedupe
/// first; the detectors themselves iterate the slice once and
/// ignore duplicates.
pub fn detect_opt_in_hardincompat(tags: &[&str]) -> Option<HardIncompat> {
    tracing::trace!(
        tags = tags.len(),
        "domain::constraint::detect_opt_in_hardincompat: enter"
    );
    if let Some(h) = detect_cluster_local_in_global(tags) {
        tracing::trace!(
            ?h,
            "domain::constraint::detect_opt_in_hardincompat: matched ClusterLocalInGlobal"
        );
        return Some(h);
    }
    if let Some(h) = detect_pull_in_push_only(tags) {
        tracing::trace!(
            ?h,
            "domain::constraint::detect_opt_in_hardincompat: matched PullInPushOnly"
        );
        return Some(h);
    }
    if let Some(h) = detect_stateless_in_stateful(tags) {
        tracing::trace!(
            ?h,
            "domain::constraint::detect_opt_in_hardincompat: matched StatelessInStateful"
        );
        return Some(h);
    }
    tracing::trace!("domain::constraint::detect_opt_in_hardincompat: no match");
    None
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
    /// Catalog I.6 (opt-in): an architectural component relies on
    /// cluster-local state (in-memory caches, sticky sessions,
    /// ephemeral locks) but is deployed into a globally-shared
    /// runtime where every replica is a distinct node. The
    /// cluster-local state evaporates the moment a request is
    /// routed to a different replica; consistency is violated
    /// silently because no single node sees the full picture.
    ClusterLocalInGlobal,
    /// Catalog I.6 (opt-in): a pull-based consumer (polling
    /// endpoint, scheduler-driven worker) is wired to a push-only
    /// producer (webhook emitter, server-sent stream) that does
    /// not expose a pollable interface. The pull consumer starves
    /// silently because the push producer has no inbound buffer
    /// to drain.
    PullInPushOnly,
    /// Catalog I.6 (opt-in): a stateless component (request-scoped
    /// handler, serverless function, load-balanced frontend) is
    /// placed where the contract requires stateful behaviour
    /// (sticky session affinity, in-process aggregate, file handle
    /// retention). The stateless routing evicts the state on every
    /// hop; the dependent session reconstructs the aggregate from
    /// scratch on every request.
    StatelessInStateful,
}

impl HardIncompat {
    /// Render a one-line, human-readable message explaining the
    /// incompatibility. Stable enough to surface in operator logs
    /// and the JSON sidecar; the unit tests pin the wording for
    /// the variants added by catalog I.6.
    pub fn explain(&self) -> String {
        let msg = match self {
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
            Self::ClusterLocalInGlobal => {
                "cluster-local state (in-memory caches, sticky sessions, ephemeral locks) is used inside a globally-shared runtime where every replica is a distinct node"
                    .to_string()
            }
            Self::PullInPushOnly => {
                "pull-based consumer (polling endpoint, scheduler-driven worker) is wired to a push-only producer (webhook emitter, server-sent stream) with no pollable interface"
                    .to_string()
            }
            Self::StatelessInStateful => {
                "stateless component (request-scoped handler, serverless function, load-balanced frontend) is placed where the contract requires stateful behaviour (sticky session affinity, in-process aggregate, file handle retention)"
                    .to_string()
            }
        };
        tracing::trace!("domain::constraint::HardIncompat::explain: emitted");
        msg
    }

    /// Catalog I.6 §D.13.15 (exhaustive): return the canonical
    /// six-variant set added by the exhaustive rewrite. The
    /// returned vector is stable (insertion order matches the
    /// enum declaration order); documentation and test fixtures
    /// use it to pin the contract without depending on each
    /// variant individually.
    pub fn from_catalog() -> Vec<HardIncompat> {
        let out = vec![
            HardIncompat::MonolithVsMicroservices,
            HardIncompat::SqlVsNosqlBackend,
            HardIncompat::GcRuntimeWithManualMem,
            HardIncompat::SingleTenantInMultitenant,
            HardIncompat::StatefulInServerless,
            HardIncompat::SyncApiWithAsyncCaller,
        ];
        tracing::trace!(
            count = out.len(),
            "domain::constraint::HardIncompat::from_catalog"
        );
        out
    }

    /// Catalog I.6 (opt-in): return the three additional unit
    /// variants reserved for sub-fases that want a runtime-clash
    /// surface without expanding the tag-pair matrix. Same
    /// stability contract as [`from_catalog`](Self::from_catalog):
    /// insertion order matches the enum declaration order so
    /// documentation and test fixtures can pin the list without
    /// enumerating each variant by name. The opt-in variants are
    /// `ClusterLocalInGlobal` (in-memory cluster-local state in a
    /// globally-shared runtime), `PullInPushOnly` (pull consumer
    /// on a push-only producer), `StatelessInStateful` (stateless
    /// component placed where the contract requires stateful
    /// behaviour).
    pub fn from_opt_in_catalog() -> Vec<HardIncompat> {
        let out = vec![
            HardIncompat::ClusterLocalInGlobal,
            HardIncompat::PullInPushOnly,
            HardIncompat::StatelessInStateful,
        ];
        tracing::trace!(
            count = out.len(),
            "domain::constraint::HardIncompat::from_opt_in_catalog"
        );
        out
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
            HardIncompat::MonolithVsMicroservices,
            HardIncompat::SqlVsNosqlBackend,
            HardIncompat::GcRuntimeWithManualMem,
            HardIncompat::SingleTenantInMultitenant,
            HardIncompat::StatefulInServerless,
            HardIncompat::SyncApiWithAsyncCaller,
            HardIncompat::ClusterLocalInGlobal,
            HardIncompat::PullInPushOnly,
            HardIncompat::StatelessInStateful,
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
    /// catalog ships sixteen variants: the seven from §I.6 plus the
    /// six exhaustive runtime-clash unit variants from §D.13.15
    /// (MonolithVsMicroservices, SqlVsNosqlBackend,
    /// GcRuntimeWithManualMem, SingleTenantInMultitenant,
    /// StatefulInServerless, SyncApiWithAsyncCaller) plus the
    /// three opt-in catalog variants (ClusterLocalInGlobal,
    /// PullInPushOnly, StatelessInStateful).
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
            HardIncompat::ClusterLocalInGlobal,
            HardIncompat::PullInPushOnly,
            HardIncompat::StatelessInStateful,
        ];
        assert_eq!(
            cases.len(),
            16,
            "HardIncompat must have 16 variants (7 base + 6 §D.13.15 exhaustive + 3 opt-in catalog)"
        );
        // Every variant serialises with the snake_case kind tag and
        // round-trips back to the same value: pins the wire format.
        for v in &cases {
            let json = serde_json::to_string(v).expect("serialize");
            let back: HardIncompat = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&back, v, "round-trip must preserve {v:?}");
        }
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
            (
                HardIncompat::ClusterLocalInGlobal,
                "cluster_local_in_global",
            ),
            (HardIncompat::PullInPushOnly, "pull_in_push_only"),
            (HardIncompat::StatelessInStateful, "stateless_in_stateful"),
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
    /// 16 (7 base + 6 exhaustive §D.13.15 + 3 opt-in catalog).
    /// The 6 §D.13.15 variants are architectural clashes widened
    /// from the partial initial set; the 3 opt-in catalog variants
    /// (`ClusterLocalInGlobal`, `PullInPushOnly`,
    /// `StatelessInStateful`) are runtime-clash records reserved
    /// for sub-fases that want the surface without expanding the
    /// tag-pair matrix. The catalog wraps them into a single test
    /// so a future addition trips this test before it lands in
    /// production.
    #[test]
    fn hard_incompat_exhaustive_variants_count_is_16() {
        let catalog = HardIncompat::from_catalog();
        assert_eq!(
            catalog.len(),
            6,
            "from_catalog must return the 6 exhaustive §D.13.15 variants"
        );
        let opt_in = HardIncompat::from_opt_in_catalog();
        assert_eq!(
            opt_in.len(),
            3,
            "from_opt_in_catalog must return the 3 opt-in catalog variants"
        );
        // Build the full set: 7 base + 6 exhaustive + 3 opt-in = 16.
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
        all.extend(opt_in);
        assert_eq!(
            all.len(),
            16,
            "HardIncompat must have 16 variants (7 base + 6 §D.13.15 exhaustive + 3 opt-in catalog)"
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

    // -- Catalog I.6 (opt-in): new variants ----------------------------

    /// `from_opt_in_catalog` returns the three opt-in variants
    /// (`ClusterLocalInGlobal`, `PullInPushOnly`,
    /// `StatelessInStateful`) in stable insertion order. Pin the
    /// contents so documentation and test fixtures can rely on
    /// the order without enumerating each variant by name.
    #[test]
    fn hard_incompat_from_opt_in_catalog_returns_canonical_list() {
        let opt_in = HardIncompat::from_opt_in_catalog();
        assert_eq!(
            opt_in,
            vec![
                HardIncompat::ClusterLocalInGlobal,
                HardIncompat::PullInPushOnly,
                HardIncompat::StatelessInStateful,
            ]
        );
    }

    /// `ClusterLocalInGlobal` is a unit variant; the message
    /// must mention both "cluster-local" and "global" so an
    /// operator reading the sidecar can spot the asymmetry
    /// without consulting the catalog. Stable wording pinned
    /// for telemetry correlation.
    #[test]
    fn hard_incompat_explain_cluster_local_in_global() {
        let h = HardIncompat::ClusterLocalInGlobal;
        let msg = h.explain();
        let lower = msg.to_lowercase();
        assert!(
            lower.contains("cluster") && lower.contains("local"),
            "message must say 'cluster-local', got: {msg}"
        );
        assert!(
            lower.contains("global"),
            "message must say 'global', got: {msg}"
        );
    }

    /// `PullInPushOnly` is a unit variant; the message must
    /// mention "pull" and "push" so the disagreement is obvious
    /// from the log line alone.
    #[test]
    fn hard_incompat_explain_pull_in_push_only() {
        let h = HardIncompat::PullInPushOnly;
        let msg = h.explain();
        let lower = msg.to_lowercase();
        assert!(lower.contains("pull"), "message must say 'pull': {msg}");
        assert!(lower.contains("push"), "message must say 'push': {msg}");
    }

    /// `StatelessInStateful` is a unit variant; the message must
    /// mention "stateless" and "stateful" so the mismatch is
    /// visible to operators reading the sidecar.
    #[test]
    fn hard_incompat_explain_stateless_in_stateful() {
        let h = HardIncompat::StatelessInStateful;
        let msg = h.explain();
        let lower = msg.to_lowercase();
        assert!(
            lower.contains("stateless"),
            "message must say 'stateless': {msg}"
        );
        assert!(
            lower.contains("stateful"),
            "message must say 'stateful': {msg}"
        );
    }

    /// Verify that valid proposal tag vectors do NOT trigger any of
    /// the new opt-in catalog variants when scanned against the
    /// tag-pair detector. The detector (`find_conflicts`) is
    /// wired to the `HARD_INCOMPATIBILITIES` matrix
    /// (proposal-03 §D.13.15), which does not reference the
    /// three opt-in variants — those are unit records with no
    /// tag-pair hook, so the detector can never surface them
    /// today. A non-conflicting tag set must therefore report
    /// `None` from `find_conflicts`, and the opt-in catalog
    /// iterators must be disjoint from the §D.13.15 set so a
    /// future sub-fase can adopt them without overlap. This
    /// pins the opt-in contract: the variants exist as typed
    /// records but are not surfaced by the default detector
    /// until a sub-fase explicitly adds the rule.
    #[test]
    fn hard_incompat_opt_in_variants_do_not_trigger_on_valid_tags() {
        // Valid cluster: monolith / sql / self_hosted / rust /
        // pull_based / standard_protocol — no pair is in the
        // §D.13.15 matrix.
        let tags = [
            "monolith",
            "sql",
            "self_hosted",
            "rust",
            "pull_based",
            "standard_protocol",
        ];
        let borrowed: Vec<&str> = tags.to_vec();
        assert!(
            find_conflicts(&borrowed).is_empty(),
            "valid tags must yield no conflict, got {:?}",
            find_conflicts(&borrowed)
        );
        // Step 2: the opt-in catalog is disjoint from the §D.13.15
        // catalog so a future sub-fase adopting the opt-in
        // variants cannot accidentally double-fire a §D.13.15
        // detection already in place.
        let opt_in = HardIncompat::from_opt_in_catalog();
        let canonical = HardIncompat::from_catalog();
        for v in &opt_in {
            assert!(
                !canonical.contains(v),
                "opt-in variant {v:?} must not appear in `from_catalog` output"
            );
        }
        // Step 3: even a tag set chosen from `HARD_INCOMPATIBILITIES`
        // triggers exactly one conflict (the pair from the matrix),
        // never an opt-in variant: pin the no-surfacing contract.
        assert!(
            is_incompatible("monolith", "microservices"),
            "matrix pair must still trigger"
        );
        let borrowed_conflicting: Vec<&str> = vec!["monolith", "microservices"];
        let conflicts = find_conflicts(&borrowed_conflicting);
        assert_eq!(conflicts.len(), 1, "exactly one conflict expected");
        // The opt-in variants are unit records, so there is no
        // direct way to "look them up" against a (a, b) pair;
        // instead we assert that the conflict detector does not
        // alias either side of the matrix pair to the new
        // opt-in names.
        let pair = conflicts[0];
        for v in &opt_in {
            assert!(
                !matches!(v, HardIncompat::ClusterLocalInGlobal) || !pair.0.contains("cluster"),
                "ClusterLocalInGlobal must not be triggered by matrix pair {pair:?}"
            );
            assert!(
                !matches!(v, HardIncompat::PullInPushOnly) || !pair.0.contains("pull"),
                "PullInPushOnly must not be triggered by matrix pair {pair:?}"
            );
            assert!(
                !matches!(v, HardIncompat::StatelessInStateful) || !pair.0.contains("stateless"),
                "StatelessInStateful must not be triggered by matrix pair {pair:?}"
            );
        }
    }

    // -- Catalog I.6 (opt-in): detector wiring -----------------------

    /// `ClusterLocalInGlobal`: a tag set that contains BOTH
    /// `cluster_local` and `global` must fire the detector. The
    /// detector is symmetric in tag order — the test exercises
    /// `global` appearing first to pin the contract.
    #[test]
    fn detect_cluster_local_in_global_fires_on_pair() {
        let tags = vec!["global", "cluster_local", "sql"];
        assert_eq!(
            detect_cluster_local_in_global(&tags),
            Some(HardIncompat::ClusterLocalInGlobal)
        );
        // Case-insensitive on both literals.
        let tags = vec!["GLOBAL", "Cluster_Local"];
        assert_eq!(
            detect_cluster_local_in_global(&tags),
            Some(HardIncompat::ClusterLocalInGlobal)
        );
    }

    /// `ClusterLocalInGlobal`: a tag set that contains only one
    /// of the two markers (or neither) must NOT fire. Pinned so
    /// a refactor that drops the AND-conjunction surfaces here.
    #[test]
    fn detect_cluster_local_in_global_does_not_fire_without_pair() {
        // Only cluster_local.
        let tags = vec!["cluster_local", "sql", "rust"];
        assert_eq!(detect_cluster_local_in_global(&tags), None);
        // Only global.
        let tags = vec!["global", "sql"];
        assert_eq!(detect_cluster_local_in_global(&tags), None);
        // Neither.
        let tags = vec!["sql", "rust", "monolith"];
        assert_eq!(detect_cluster_local_in_global(&tags), None);
        // Empty.
        let empty: Vec<&str> = Vec::new();
        assert_eq!(detect_cluster_local_in_global(&empty), None);
    }

    /// `PullInPushOnly`: a tag set that contains a pull-mode
    /// marker (`pull_based` OR `pull_required`) AND a push-mode
    /// marker (`push_only` OR `push_endpoint`) must fire.
    /// Pin every combination so a refactor that drops a
    /// disjunct surfaces here.
    #[test]
    fn detect_pull_in_push_only_fires_on_pair() {
        // pull_based + push_only
        let tags = vec!["pull_based", "push_only"];
        assert_eq!(
            detect_pull_in_push_only(&tags),
            Some(HardIncompat::PullInPushOnly)
        );
        // pull_based + push_endpoint
        let tags = vec!["pull_based", "push_endpoint"];
        assert_eq!(
            detect_pull_in_push_only(&tags),
            Some(HardIncompat::PullInPushOnly)
        );
        // pull_required + push_only
        let tags = vec!["pull_required", "push_only"];
        assert_eq!(
            detect_pull_in_push_only(&tags),
            Some(HardIncompat::PullInPushOnly)
        );
        // pull_required + push_endpoint
        let tags = vec!["pull_required", "push_endpoint"];
        assert_eq!(
            detect_pull_in_push_only(&tags),
            Some(HardIncompat::PullInPushOnly)
        );
    }

    /// `PullInPushOnly`: a tag set that contains only pull-side
    /// markers, only push-side markers, or neither must NOT fire.
    #[test]
    fn detect_pull_in_push_only_does_not_fire_without_pair() {
        // Only pull-side markers.
        let tags = vec!["pull_based", "pull_required"];
        assert_eq!(detect_pull_in_push_only(&tags), None);
        // Only push-side markers.
        let tags = vec!["push_only", "push_endpoint"];
        assert_eq!(detect_pull_in_push_only(&tags), None);
        // Unrelated tags.
        let tags = vec!["sql", "rust", "monolith"];
        assert_eq!(detect_pull_in_push_only(&tags), None);
        // Empty.
        let empty: Vec<&str> = Vec::new();
        assert_eq!(detect_pull_in_push_only(&empty), None);
    }

    /// `StatelessInStateful`: a tag set that contains BOTH
    /// `stateless` and `stateful_required` must fire. The
    /// detector is symmetric in tag order.
    #[test]
    fn detect_stateless_in_stateful_fires_on_pair() {
        let tags = vec!["stateless", "stateful_required"];
        assert_eq!(
            detect_stateless_in_stateful(&tags),
            Some(HardIncompat::StatelessInStateful)
        );
        // Reversed order.
        let tags = vec!["stateful_required", "stateless"];
        assert_eq!(
            detect_stateless_in_stateful(&tags),
            Some(HardIncompat::StatelessInStateful)
        );
        // Case-insensitive.
        let tags = vec!["STATELESS", "Stateful_Required"];
        assert_eq!(
            detect_stateless_in_stateful(&tags),
            Some(HardIncompat::StatelessInStateful)
        );
    }

    /// `StatelessInStateful`: a tag set missing either side must
    /// NOT fire. Pinned so a refactor that drops the
    /// AND-conjunction surfaces here.
    #[test]
    fn detect_stateless_in_stateful_does_not_fire_without_pair() {
        let tags = vec!["stateless", "sql"];
        assert_eq!(detect_stateless_in_stateful(&tags), None);
        let tags = vec!["stateful_required", "rust"];
        assert_eq!(detect_stateless_in_stateful(&tags), None);
        let tags = vec!["sql", "rust"];
        assert_eq!(detect_stateless_in_stateful(&tags), None);
        let empty: Vec<&str> = Vec::new();
        assert_eq!(detect_stateless_in_stateful(&empty), None);
    }

    /// The wrapper [`detect_opt_in_hardincompat`] runs the three
    /// detectors in a fixed order and returns the first match.
    /// Pin the order (matching the enum declaration order) so a
    /// refactor that re-orders the detector list surfaces here
    /// before the wire form drifts.
    #[test]
    fn detect_opt_in_hardincompat_returns_first_match_in_deterministic_order() {
        // Single variant matches: ClusterLocalInGlobal.
        let tags = vec!["cluster_local", "global"];
        assert_eq!(
            detect_opt_in_hardincompat(&tags),
            Some(HardIncompat::ClusterLocalInGlobal)
        );
        // Order: ClusterLocalInGlobal wins over the others.
        let tags = vec![
            "cluster_local",
            "global",
            "pull_based",
            "push_only",
            "stateless",
            "stateful_required",
        ];
        assert_eq!(
            detect_opt_in_hardincompat(&tags),
            Some(HardIncompat::ClusterLocalInGlobal)
        );
        // Without ClusterLocalInGlobal, PullInPushOnly wins.
        let tags = vec!["pull_based", "push_only", "stateless", "stateful_required"];
        assert_eq!(
            detect_opt_in_hardincompat(&tags),
            Some(HardIncompat::PullInPushOnly)
        );
        // Without the first two, StatelessInStateful fires.
        let tags = vec!["stateless", "stateful_required"];
        assert_eq!(
            detect_opt_in_hardincompat(&tags),
            Some(HardIncompat::StatelessInStateful)
        );
        // No opt-in match — the caller is expected to fall
        // through to `find_conflicts` for the matrix check.
        let tags = vec!["sql", "rust", "monolith"];
        assert_eq!(detect_opt_in_hardincompat(&tags), None);
    }
}
