//! The discover pipeline shape and the run spec resolved from the CLI.

use super::*;
use crate::phases::PipelineKind;

fn profile(temperatures: &[f32], replicas: usize) -> TemperatureProfile {
    TemperatureProfile {
        temperatures: temperatures.to_vec(),
        replicas_per_temperature: replicas,
    }
}

fn spec_with_matrix(matrix_spec: &[&str]) -> DiscoverRunSpec {
    DiscoverRunSpec {
        provider: "mock:mock-model".to_owned(),
        mock_dir: None,
        max_parallelism: None,
        sketches_per_cell: 3,
        matrix_spec: matrix_spec.iter().map(|s| (*s).to_owned()).collect(),
        dimensions: None,
        facets_per_dimension: None,
        llm_derive: false,
        temperature_profiles: BTreeMap::new(),
        default_profile: None,
    }
}

fn options() -> DiscoverOptions {
    DiscoverOptions {
        provider: "minimax:MiniMax-M3".to_owned(),
        prompt: "Diseña el catálogo.".to_owned(),
        sketches_per_cell: 2,
        ..DiscoverOptions::default()
    }
}

#[test]
fn a_fixed_matrix_runs_intake_sketches_curate_and_render() {
    let pipeline = discover_pipeline(&spec_with_matrix(&["auth=oauth,api-key"]), false);
    assert_eq!(
        pipeline.names(),
        [
            "intake",
            "discover_sketches",
            "discover_curate",
            "discover_render"
        ]
    );
}

#[test]
fn a_derived_matrix_runs_the_dimensions_phase_until_the_matrix_is_persisted() {
    let spec = spec_with_matrix(&[]);
    assert_eq!(
        discover_pipeline(&spec, false).names(),
        [
            "intake",
            "discover_dimensions",
            "discover_sketches",
            "discover_curate",
            "discover_render"
        ]
    );
    assert_eq!(
        discover_pipeline(&spec, true).names(),
        [
            "intake",
            "discover_sketches",
            "discover_curate",
            "discover_render"
        ]
    );
}

#[test]
fn the_full_pipeline_is_the_canonical_discovery_order() {
    assert_eq!(
        discover_pipeline(&spec_with_matrix(&[]), false).names(),
        Pipeline::canonical_phase_order_for(PipelineKind::Discovery)
    );
}

#[test]
fn the_spec_takes_every_choice_from_the_cli() {
    let mut opts = options();
    opts.mock_dir = Some(PathBuf::from("/fixtures/mock"));
    opts.max_parallelism = Some(16);
    opts.matrix_spec = vec!["auth=oauth,api-key".to_owned()];
    opts.dimensions = Some(4);
    opts.facets_per_dimension = Some(2);
    opts.llm_derive = true;
    let spec = resolve_spec(&opts, &Config::default()).unwrap();
    assert_eq!(spec.provider, "minimax:MiniMax-M3");
    assert_eq!(spec.mock_dir, Some(PathBuf::from("/fixtures/mock")));
    assert_eq!(spec.max_parallelism, Some(16));
    assert_eq!(spec.sketches_per_cell, 2);
    assert_eq!(spec.matrix_spec, ["auth=oauth,api-key"]);
    assert_eq!(
        (spec.dimensions, spec.facets_per_dimension),
        (Some(4), Some(2))
    );
    assert!(spec.llm_derive);
}

#[test]
fn the_config_fills_what_the_cli_leaves_out() {
    let mut cfg = Config {
        default_provider: "deepseek:v4".to_owned(),
        ..Config::default()
    };
    cfg.discovery_matrix.matrix_spec = vec!["pricing=list,margin".to_owned()];
    cfg.discovery_matrix.dimensions = Some(3);
    cfg.discovery_matrix.facets_per_dimension = Some(5);
    cfg.discovery_matrix.llm_derive_first = true;
    cfg.discovery_matrix.default_profile = Some(profile(&[0.4], 2));
    let mut opts = options();
    opts.provider = String::new();
    let spec = resolve_spec(&opts, &cfg).unwrap();
    assert_eq!(spec.provider, "deepseek:v4");
    assert_eq!(spec.matrix_spec, ["pricing=list,margin"]);
    assert_eq!(
        (spec.dimensions, spec.facets_per_dimension),
        (Some(3), Some(5))
    );
    assert!(spec.llm_derive);
    assert_eq!(spec.default_profile, Some(profile(&[0.4], 2)));
    assert_eq!(spec.sketches_per_cell, 2, "the CLI value always wins");
}

#[test]
fn cli_temperature_profiles_override_the_config_under_their_joined_key() {
    let mut cfg = Config::default();
    cfg.discovery_matrix
        .temperature_profiles
        .insert("minimax::MiniMax-M3".to_owned(), profile(&[0.1], 1));
    cfg.discovery_matrix
        .temperature_profiles
        .insert("other::kept".to_owned(), profile(&[0.2], 1));
    let mut opts = options();
    opts.temperature_profiles = vec![
        TemperatureProfileSpec::parse("provider=MiniMax-M3;temperatures=0.5;replicas=1").unwrap(),
        TemperatureProfileSpec::parse("provider=MiniMax-M3;temperatures=0.7,1.0;replicas=2")
            .unwrap(),
        TemperatureProfileSpec::parse("provider=opencode:mimo;temperatures=0.9;replicas=1")
            .unwrap(),
    ];
    let spec = resolve_spec(&opts, &cfg).unwrap();
    assert_eq!(
        spec.temperature_profiles,
        BTreeMap::from([
            ("minimax::MiniMax-M3".to_owned(), profile(&[0.7, 1.0], 2)),
            ("opencode::mimo".to_owned(), profile(&[0.9], 1)),
            ("other::kept".to_owned(), profile(&[0.2], 1)),
        ])
    );
}

#[test]
fn a_bare_provider_section_is_rejected() {
    let mut opts = options();
    opts.provider = "minimax".to_owned();
    let err = resolve_spec(&opts, &Config::default()).unwrap_err();
    assert!(matches!(err, Error::InvalidArgs(_)), "{err:?}");
    assert!(err.to_string().contains("SECTION:MODEL"), "{err}");
}

#[test]
fn a_malformed_matrix_spec_is_rejected() {
    let mut opts = options();
    opts.matrix_spec = vec!["auth".to_owned()];
    assert!(resolve_spec(&opts, &Config::default()).is_err());
}

#[test]
fn resolving_the_same_options_twice_gives_the_same_spec() {
    let mut opts = options();
    opts.temperature_profiles = vec![
        TemperatureProfileSpec::parse("provider=a:x;temperatures=0.5;replicas=1").unwrap(),
        TemperatureProfileSpec::parse("provider=b:y;temperatures=0.6;replicas=1").unwrap(),
    ];
    let cfg = Config::default();
    assert_eq!(
        resolve_spec(&opts, &cfg).unwrap(),
        resolve_spec(&opts, &cfg).unwrap()
    );
}
