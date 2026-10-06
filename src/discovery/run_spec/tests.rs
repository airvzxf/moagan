//! Persistence and interpretation of the discover run spec.

use super::*;

fn profile(temperatures: &[f32], replicas: usize) -> TemperatureProfile {
    TemperatureProfile {
        temperatures: temperatures.to_vec(),
        replicas_per_temperature: replicas,
    }
}

fn spec() -> DiscoverRunSpec {
    DiscoverRunSpec {
        provider: "minimax:MiniMax-M3".to_owned(),
        mock_dir: None,
        max_parallelism: Some(16),
        sketches_per_cell: 2,
        matrix_spec: vec!["auth=oauth,api-key".to_owned()],
        dimensions: None,
        facets_per_dimension: None,
        llm_derive: false,
        temperature_profiles: BTreeMap::from([(
            "minimax::MiniMax-M3".to_owned(),
            profile(&[0.7, 1.0], 1),
        )]),
        default_profile: None,
    }
}

#[test]
fn a_saved_spec_loads_back_equal() {
    let dir = tempfile::tempdir().unwrap();
    let mut original = spec();
    original.mock_dir = Some(PathBuf::from("/fixtures/mock"));
    original.default_profile = Some(profile(&[1.0], 1));
    original.save(dir.path()).unwrap();
    assert!(dir.path().join(DISCOVER_RUN_FILENAME).is_file());
    assert_eq!(DiscoverRunSpec::load(dir.path()).unwrap(), original);
}

#[test]
fn saving_the_same_spec_twice_writes_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec();
    s.temperature_profiles
        .insert("deepseek::v4".to_owned(), profile(&[0.2], 3));
    s.temperature_profiles
        .insert("anthropic::m".to_owned(), profile(&[0.5], 1));
    s.save(dir.path()).unwrap();
    let first = std::fs::read(dir.path().join(DISCOVER_RUN_FILENAME)).unwrap();
    s.save(dir.path()).unwrap();
    let second = std::fs::read(dir.path().join(DISCOVER_RUN_FILENAME)).unwrap();
    assert_eq!(first, second);
}

#[test]
fn a_run_without_a_spec_is_rebuilt_from_its_exploration_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let mut matrix = ExplorationMatrix::new(Vec::new(), 10);
    matrix.temperature_profiles.insert(
        "minimax::MiniMax-M3".to_owned(),
        profile(&[0.0, 0.7, 1.9], 1),
    );
    write_json(&dir.path().join("exploration_matrix.json"), &matrix).unwrap();
    let loaded = DiscoverRunSpec::load(dir.path()).unwrap();
    assert_eq!(loaded.provider, "minimax:MiniMax-M3");
    assert_eq!(loaded.sketches_per_cell, 10);
    assert_eq!(
        loaded.temperature_profiles,
        BTreeMap::from([(
            "minimax::MiniMax-M3".to_owned(),
            profile(&[0.0, 0.7, 1.9], 1)
        )])
    );
    assert_eq!(loaded.default_profile, Some(TemperatureProfile::default()));
    assert!(loaded.matrix_spec.is_empty());
    assert_eq!(loaded.mock_dir, None);
    assert_eq!(loaded.max_parallelism, None);
}

#[test]
fn the_legacy_provider_is_the_first_joined_profile_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut matrix = ExplorationMatrix::new(Vec::new(), 3);
    for key in ["zeta::z1", "MiniMax-M3", "alpha::a1"] {
        matrix
            .temperature_profiles
            .insert(key.to_owned(), profile(&[1.0], 1));
    }
    write_json(&dir.path().join("exploration_matrix.json"), &matrix).unwrap();
    assert_eq!(
        DiscoverRunSpec::load(dir.path()).unwrap().provider,
        "alpha:a1"
    );
}

#[test]
fn a_spec_file_wins_over_the_exploration_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let mut matrix = ExplorationMatrix::new(Vec::new(), 9);
    matrix
        .temperature_profiles
        .insert("other::m".to_owned(), profile(&[1.0], 1));
    write_json(&dir.path().join("exploration_matrix.json"), &matrix).unwrap();
    spec().save(dir.path()).unwrap();
    assert_eq!(DiscoverRunSpec::load(dir.path()).unwrap(), spec());
}

#[test]
fn a_run_dir_with_neither_file_cannot_be_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let err = DiscoverRunSpec::load(dir.path()).unwrap_err();
    assert!(matches!(err, Error::InvalidState(_)), "{err:?}");
    assert!(err.to_string().contains(DISCOVER_RUN_FILENAME), "{err}");
}

#[test]
fn a_legacy_matrix_without_a_joined_profile_key_cannot_be_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let mut matrix = ExplorationMatrix::new(Vec::new(), 2);
    matrix
        .temperature_profiles
        .insert("MiniMax-M3".to_owned(), profile(&[1.0], 1));
    write_json(&dir.path().join("exploration_matrix.json"), &matrix).unwrap();
    let err = DiscoverRunSpec::load(dir.path()).unwrap_err();
    assert!(matches!(err, Error::InvalidState(_)), "{err:?}");
    assert!(err.to_string().contains("provider"), "{err}");
}

#[test]
fn dimensions_are_derived_only_without_a_spec_and_without_both_counts() {
    let mut s = spec();
    assert!(!s.derives_dimensions(), "a matrix spec fixes the matrix");
    s.llm_derive = true;
    assert!(!s.derives_dimensions(), "the spec wins over --llm-derive");
    s.matrix_spec = vec!["  ".to_owned()];
    assert!(s.derives_dimensions(), "a blank entry is no spec");
    s.llm_derive = false;
    s.dimensions = Some(4);
    assert!(
        s.derives_dimensions(),
        "a dimension count alone still derives"
    );
    s.facets_per_dimension = Some(2);
    assert!(!s.derives_dimensions(), "both counts fix the matrix");
    s.llm_derive = true;
    assert!(s.derives_dimensions(), "--llm-derive wins over the counts");
}

#[test]
fn applying_the_spec_overwrites_every_matrix_knob_of_the_config() {
    let mut cfg = DiscoveryMatrixConfig {
        sketches_per_cell: 10,
        matrix_spec: vec!["old=a,b".to_owned()],
        dimensions: Some(8),
        facets_per_dimension: Some(1),
        llm_derive_first: true,
        default_profile: Some(profile(&[0.1], 1)),
        ..DiscoveryMatrixConfig::default()
    };
    cfg.temperature_profiles
        .insert("stale::model".to_owned(), profile(&[0.4], 1));
    let s = spec();
    s.apply_to(&mut cfg);
    assert_eq!(cfg.sketches_per_cell, 2);
    assert_eq!(cfg.matrix_spec, ["auth=oauth,api-key"]);
    assert_eq!(cfg.dimensions, None);
    assert_eq!(cfg.facets_per_dimension, None);
    assert!(!cfg.llm_derive_first);
    assert_eq!(cfg.default_profile, None);
    let keys: Vec<&String> = cfg.temperature_profiles.keys().collect();
    assert_eq!(keys, ["minimax::MiniMax-M3"]);
    assert_eq!(
        cfg.temperature_profiles["minimax::MiniMax-M3"],
        profile(&[0.7, 1.0], 1)
    );
}
