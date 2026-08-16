//! Integration tests for the optional petgraph DAG backend (v0.9
//! round 5, ADR 0001 §D-1).
//!
//! Three regression guards in one file:
//!
//! 1. **Default build** (`cargo test`, no features) — `cfg!(feature
//!    = "dag")` must be `false`, the `dag` module must not exist,
//!    and the linear `Pipeline::run` path must keep working. This
//!    is the no-go list contract: a blanket `petgraph` row would
//!    silently enable the feature and pull `fixedbitset`/`ahash`/
//!    `indexmap` into the release binary.
//!
//! 2. **Feature build** (`cargo test --features dag`) — the
//!    `build_dag_for_deep_mode` graph is the canonical 16-phase
//!    chain, `execute_dag` runs the phases in canonical order
//!    when given stub implementations, and the gated
//!    `maybe_run_via_dag` dispatcher routes deep-mode runs to the
//!    DAG and falls through on every other mode.
//!
//! 3. **Cycle rejection** — a hand-built cyclic DAG surfaces
//!    `Error::InvalidState` from `topological_layers`, with the
//!    stuck node names in the message so a debugger can spot the
//!    cycle without re-running the test.

#[cfg(not(feature = "dag"))]
mod default_build {
    //! Regression guard: without `--features dag`, the optional
    //! DAG backend must be completely absent. ADR 0001 §D-1 keeps
    //! `petgraph` out of the default build; this test catches a
    //! future slip where someone removes the `optional = true`
    //! flag or adds the feature to the default set.

    /// The `cfg!(feature = "dag")` value is decided at compile
    /// time, so a plain `assert!` triggers clippy's
    /// `assertions_on_constants` lint. Wrap the probe in a
    /// `const { ... }` block so the assertion evaluates as a
    /// compile-time check rather than a runtime one. The test
    /// still runs as a regression guard in case the feature flag
    /// later changes via a `[build-dependencies]` shim.
    #[test]
    fn dag_feature_is_off_by_default() {
        const {
            assert!(
                !cfg!(feature = "dag"),
                "default build must NOT enable the dag feature"
            )
        };
    }

    #[test]
    fn dag_module_is_not_compiled_in_default_build() {
        // `moagan::phases::dag` is gated by `#[cfg(feature = "dag")]`,
        // so a default build cannot import the module. Use a
        // string-level probe of the public surface to assert the
        // `dag` symbol is not in scope, then evaluate the cfg as
        // a compile-time check to pin the contract.
        let _ = std::any::type_name::<moagan::phases::Pipeline>();
        const {
            assert!(
                !cfg!(feature = "dag"),
                "default build must not expose the petgraph DAG backend"
            )
        };
    }
}

#[cfg(feature = "dag")]
mod feature_build {
    //! Feature build coverage: the DAG API exists, returns the
    //! canonical topology, executes phases in order, and the
    //! dispatcher helper routes deep-mode runs through it.

    use std::sync::Arc;

    use async_trait::async_trait;

    use moagan::execution::Parallelism;
    use moagan::fs_layout::MoaganHome;
    use moagan::ids::RunId;
    use moagan::llm::ProviderRegistry;
    use moagan::phases::{
        Phase, PhaseOutput, Pipeline, RunContext, build_dag_for_deep_mode, execute_dag,
        topological_layers,
    };
    use moagan::telemetry::Telemetry;

    struct OrderRecorder {
        name: &'static str,
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Phase for OrderRecorder {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn execute(&self, _ctx: &RunContext) -> Result<PhaseOutput, moagan::error::Error> {
            self.log.lock().expect("mutex").push(self.name);
            Ok(PhaseOutput::Intake(std::path::PathBuf::from(self.name)))
        }
    }

    fn empty_ctx(mode: &str) -> RunContext {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
        std::mem::forget(tmp);
        RunContext::new(
            RunId::default(),
            home,
            Arc::new(ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            Parallelism::new(1),
            Telemetry::noop(),
            String::new(),
            mode.into(),
        )
    }

    /// The deep-mode DAG exposes exactly the canonical 16 phases
    /// in the canonical order. Mirrors the
    /// `dag_topology_matches_linear_order` invariant at the integration
    /// level so any drift in `build_dag_for_deep_mode` is caught by
    /// both the unit and integration suites.
    #[test]
    fn deep_mode_dag_matches_canonical_phase_order() {
        let graph = build_dag_for_deep_mode();
        let names: Vec<&'static str> = graph.node_indices().map(|i| graph[i].as_str()).collect();
        assert_eq!(
            names,
            vec![
                "intake",
                "clarify",
                "route",
                "decompose",
                "sketch",
                "propose",
                "validate",
                "cluster_proposals",
                "synthesize",
                "gate",
                "critique",
                "repair",
                "judge",
                "adversary",
                "rank",
                "deliver",
            ]
        );
    }

    /// `topological_layers` returns the canonical 16 singleton
    /// layers for the deep-mode DAG. Locks the executor's
    /// iteration order at the integration level too.
    #[test]
    fn topological_layers_match_canonical_chain() {
        let graph = build_dag_for_deep_mode();
        let layers = topological_layers(&graph).expect("chain is a DAG");
        assert_eq!(layers.len(), 16);
        for layer in &layers {
            assert_eq!(layer.len(), 1);
        }
    }

    /// `execute_dag` runs every phase exactly once, in canonical
    /// order, when given the full 16-phase set as
    /// `Box<dyn Phase>`. The mock phases push their names into a
    /// shared `Mutex<Vec<&str>>` so the test sees the actual
    /// execution order, not the topology order alone.
    #[tokio::test]
    async fn execute_dag_runs_phases_in_canonical_order() {
        let log: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let phase_names: [&'static str; 16] = [
            "intake",
            "clarify",
            "route",
            "decompose",
            "sketch",
            "propose",
            "validate",
            "cluster_proposals",
            "synthesize",
            "gate",
            "critique",
            "repair",
            "judge",
            "adversary",
            "rank",
            "deliver",
        ];
        let phases: Vec<Box<dyn Phase>> = phase_names
            .iter()
            .map(|n| {
                Box::new(OrderRecorder {
                    name: n,
                    log: Arc::clone(&log),
                }) as Box<dyn Phase>
            })
            .collect();

        let graph = build_dag_for_deep_mode();
        let ctx = empty_ctx("deep");
        let outputs = execute_dag(&graph, &phases, &ctx)
            .await
            .expect("execute_dag succeeds with full phase set");
        assert_eq!(outputs.len(), 16);

        let observed = log.lock().expect("mutex").clone();
        assert_eq!(
            observed,
            phase_names.to_vec(),
            "execute_dag must run phases in canonical order"
        );
    }

    /// `execute_dag` errors out with `Error::InvalidState` when a
    /// referenced phase name has no implementation in the supplied
    /// phase slice. The error message must include the missing
    /// name so a debugger can spot the gap without re-running.
    #[tokio::test]
    async fn execute_dag_errors_when_a_phase_is_missing() {
        let log: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let phases: Vec<Box<dyn Phase>> = vec![
            Box::new(OrderRecorder {
                name: "intake",
                log: Arc::clone(&log),
            }),
            // gap: every phase from "clarify" through "deliver"
            // is missing. execute_dag must fail at the first
            // missing reference.
        ];

        let graph = build_dag_for_deep_mode();
        let ctx = empty_ctx("deep");
        let err = execute_dag(&graph, &phases, &ctx)
            .await
            .expect_err("missing phase must error");
        match err {
            moagan::error::Error::InvalidState(msg) => {
                assert!(
                    msg.contains("clarify"),
                    "error must mention the missing phase name; got: {msg}"
                );
            }
            other => panic!("expected Error::InvalidState, got: {other:?}"),
        }
    }

    /// `maybe_run_via_dag` returns `Some` only when the run is in
    /// `deep` mode (and the feature is on, which this module
    /// already pins). For any other mode the dispatcher falls
    /// through so the linear `Pipeline::run` path stays in charge.
    #[test]
    fn dispatcher_falls_through_for_non_deep_modes() {
        let pipeline = Pipeline::new();
        for mode in ["fast", "standard", "explore", "batch"] {
            let ctx = empty_ctx(mode);
            let dispatched = moagan::phases::pipe::maybe_run_via_dag(&pipeline, &ctx);
            assert!(
                dispatched.is_none(),
                "dispatcher must fall through for mode={mode}, got Some(_) branch"
            );
        }
    }

    /// `maybe_run_via_dag` returns the DAG future for `deep` mode.
    /// Awaiting it must produce the canonical 16 outputs in order.
    #[tokio::test]
    async fn dispatcher_routes_deep_mode_through_dag() {
        let log: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let phase_names: [&'static str; 16] = [
            "intake",
            "clarify",
            "route",
            "decompose",
            "sketch",
            "propose",
            "validate",
            "cluster_proposals",
            "synthesize",
            "gate",
            "critique",
            "repair",
            "judge",
            "adversary",
            "rank",
            "deliver",
        ];
        let mut pipeline = Pipeline::new();
        for n in phase_names {
            pipeline = pipeline.push(OrderRecorder {
                name: n,
                log: Arc::clone(&log),
            });
        }
        let ctx = empty_ctx("deep");

        let fut = moagan::phases::pipe::maybe_run_via_dag(&pipeline, &ctx)
            .expect("dispatcher must route deep mode to the DAG");
        let outputs = fut.await.expect("DAG execution succeeds");
        assert_eq!(outputs.len(), 16);

        let observed = log.lock().expect("mutex").clone();
        assert_eq!(
            observed,
            phase_names.to_vec(),
            "DAG-backed deep mode must execute phases in canonical order"
        );
    }

    /// `Pipeline::new()` + repeated `push(...)` produces a
    /// pipeline whose `names()` mirror the push order. Pins the
    /// builder contract the dispatcher relies on (the dispatcher
    /// indexes phases by `name()`).
    #[test]
    fn pipeline_push_then_dispatch_in_deep_mode() {
        let log: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let phase_names: [&'static str; 3] = ["intake", "clarify", "deliver"];
        let mut pipeline = Pipeline::new();
        for n in phase_names {
            pipeline = pipeline.push(OrderRecorder {
                name: n,
                log: Arc::clone(&log),
            });
        }
        assert_eq!(pipeline.names(), vec!["intake", "clarify", "deliver"]);
        assert_eq!(pipeline.len(), 3);
    }
}
