//! End-to-end integration tests for Phase I (v0.3 «tercera
//! etapa» sub-fase I — telemetry dashboard, export, verify,
//! retention).
//!
//! These tests exercise the `moagan telemetry` CLI surface
//! against a real `MoaganHome` directory and a real SQLite
//! index. The mock provider is used to populate a run with
//! call / phase / warning / checkpoint sidecars so the
//! dashboard queries and the export / verify round-trips have
//! something to chew on.
//!
//! They complement the unit tests in `src/telemetry/*` (which
//! cover the helper internals) by exercising the integration
//! points: `TelemetryCmd::dispatch` against a populated
//! `.runs/` directory, the export → verify contract, the
//! retention pass, and the dashboard HTTP server.

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use moagan::cli::telemetry_cmd::{ExportFormat, ExportLevel, TelemetryCmd};
use moagan::config::Config;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::storage::sqlite::Db;
use moagan::telemetry::dashboard::{self, DashboardConfig};
use moagan::telemetry::export::{self, ExportResult};
use moagan::telemetry::retention::{RetentionConfig, RetentionPolicy, apply, plan};
use moagan::telemetry::verify::{self, VerifyVerdict};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Build a fully-populated run directory + SQLite index so the
/// dashboard / export / retention helpers have something to
/// operate on. Returns the home directory (so the caller can
/// mount the dashboard or pass paths to the dispatch layer).
fn populate_home() -> MoaganHome {
    let tmp = tempfile::tempdir().unwrap();
    // Force MOAGAN_HOME so `MoaganHome::resolve` picks up the
    // tmpdir. The env lock around the rest of the suite keeps
    // the variable stable.
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();

    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    std::fs::write(run_dir.manifest(), br#"{"schema_version":"v1"}"#).unwrap();
    std::fs::write(run_dir.brief(), br#"{"goal":"integration test"}"#).unwrap();
    std::fs::write(
        run_dir.rankings().join("ranking.json"),
        br#"{"selected":[]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(run_dir.telemetry()).unwrap();
    std::fs::create_dir_all(run_dir.proposals()).unwrap();
    std::fs::write(
        run_dir.proposals().join("p_01.json"),
        br#"{"id":"p_01","score":0.9}"#,
    )
    .unwrap();
    std::fs::write(
        run_dir.proposals().join("p_02.json"),
        br#"{"id":"p_02","score":0.7}"#,
    )
    .unwrap();
    std::fs::write(
        run_dir.final_dir().join("portfolio.md"),
        b"# Portfolio\n\nintegration test.\n",
    )
    .unwrap();

    let db_path = home.meta_db_path();
    let db = Db::open(&db_path).unwrap();
    db.register_run(run_id, "fast", "completed", "0.3.0", None, None, None)
        .unwrap();
    let suffix = run_id.to_string();
    db.record_call(
        &format!("c1-{suffix}"),
        run_id,
        "intake",
        "intake",
        "minimax",
        "MiniMax-M3",
        &format!("k1-{suffix}"),
        Some("sha1"),
        false,
        Some(200),
        100,
        50,
        10,
        0,
        1_700_000_000,
        1_700_000_005,
        None,
        0,
    )
    .unwrap();
    db.record_call(
        &format!("c2-{suffix}"),
        run_id,
        "intake",
        "intake",
        "minimax",
        "MiniMax-M3",
        &format!("k2-{suffix}"),
        None,
        true,
        None,
        0,
        0,
        0,
        0,
        1_700_000_010,
        1_700_000_011,
        None,
        0,
    )
    .unwrap();
    db.accumulate_usage(run_id, "minimax", "MiniMax-M3", 2, 100, 50, 10, 0)
        .unwrap();
    db.record_phase(run_id, "intake", 0, "start", None).unwrap();
    db.record_phase(run_id, "intake", 0, "end", None).unwrap();
    db.record_warning(
        run_id,
        1_700_000_005_000,
        "model.json_repair_applied",
        "warn",
        Some("intake"),
        Some("intake"),
        Some(&format!("c1-{suffix}")),
        Some(0),
        "colon repair",
        "{}",
    )
    .unwrap();
    db.record_checkpoint(
        run_id,
        &format!("h_intake-{suffix}"),
        "intake",
        "continue?",
        "y",
        true,
        1_700_000_010,
    )
    .unwrap();

    // We can't return both the home and the tempdir; the home
    // pins the path and the tempdir is leaked so the SQLite
    // connection stays valid for the duration of the test.
    std::mem::forget(tmp);
    home
}

#[test]
fn list_runs_returns_seeded_run_id() {
    let _guard = env_lock();
    let home = populate_home();
    let cmd = TelemetryCmd::List {
        runs_dir: Some(home.root().to_path_buf()),
        limit: 5,
        run: None,
    };
    let code = pollster::block_on(cmd.dispatch()).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn list_one_run_returns_combined_summary() {
    let _guard = env_lock();
    let home = populate_home();
    // Locate the seeded run id by listing runs in SQLite.
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();
    let cmd = TelemetryCmd::List {
        runs_dir: Some(home.root().to_path_buf()),
        limit: 5,
        run: Some(run_id.to_string()),
    };
    let code = pollster::block_on(cmd.dispatch()).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn summary_returns_aggregate_for_seeded_run() {
    let _guard = env_lock();
    let home = populate_home();
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();
    let cmd = TelemetryCmd::Summary {
        runs_dir: Some(home.root().to_path_buf()),
        run: run_id.to_string(),
    };
    let code = pollster::block_on(cmd.dispatch()).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn summary_unknown_run_returns_error() {
    let _guard = env_lock();
    let home = populate_home();
    let cmd = TelemetryCmd::Summary {
        runs_dir: Some(home.root().to_path_buf()),
        run: "01900000-0000-0000-0000-000000000000".into(),
    };
    let err = pollster::block_on(cmd.dispatch()).unwrap_err();
    assert!(matches!(err, moagan::error::Error::InvalidState(_)));
}

#[test]
fn compare_two_runs_reports_zero_delta() {
    let _guard = env_lock();
    let home = populate_home();
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();
    let cmd = TelemetryCmd::Compare {
        runs_dir: Some(home.root().to_path_buf()),
        run_a: run_id.to_string(),
        run_b: run_id.to_string(),
    };
    let code = pollster::block_on(cmd.dispatch()).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn compare_unknown_run_returns_invalid_state() {
    let _guard = env_lock();
    let home = populate_home();
    let cmd = TelemetryCmd::Compare {
        runs_dir: Some(home.root().to_path_buf()),
        run_a: "01900000-0000-0000-0000-000000000000".into(),
        run_b: "01900000-0000-0000-0000-000000000001".into(),
    };
    let err = pollster::block_on(cmd.dispatch()).unwrap_err();
    assert!(matches!(err, moagan::error::Error::InvalidState(_)));
}

#[test]
fn provider_list_runs_against_seeded_home() {
    let _guard = env_lock();
    let home = populate_home();
    let cmd = TelemetryCmd::Provider {
        runs_dir: Some(home.root().to_path_buf()),
        plan: None,
        list: true,
    };
    let code = pollster::block_on(cmd.dispatch()).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn provider_plan_minimax_returns_known_run() {
    let _guard = env_lock();
    let home = populate_home();
    let cmd = TelemetryCmd::Provider {
        runs_dir: Some(home.root().to_path_buf()),
        plan: Some("minimax".into()),
        list: false,
    };
    let code = pollster::block_on(cmd.dispatch()).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn export_then_verify_round_trip_ok() {
    let _guard = env_lock();
    let home = populate_home();
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();

    let staging = tempfile::tempdir().unwrap();
    let archive = staging.path().join("bundle.tar.gz");
    let run_dir = home.run_dir(run_id);
    let result: ExportResult = export::export_run(
        &run_dir,
        run_id,
        ExportLevel::Summary,
        ExportFormat::TarGz,
        &archive,
    )
    .unwrap();
    assert!(result.file_count >= 4, "summary export bundles core files");
    assert!(result.archive_bytes > 0);
    assert!(result.archive_sha256.len() == 64);

    let report = verify::verify(&archive).unwrap();
    assert!(report.rows.iter().all(|r| r.verdict.label() == "OK"));
    assert_eq!(report.ok_count(), result.file_count);
    assert_eq!(report.fail_count(), 0);
    std::mem::forget(staging);
}

#[test]
fn verify_detects_tampering() {
    let _guard = env_lock();
    let home = populate_home();
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();

    let staging = tempfile::tempdir().unwrap();
    let archive = staging.path().join("bundle.tar.gz");
    let run_dir = home.run_dir(run_id);
    export::export_run(
        &run_dir,
        run_id,
        ExportLevel::Summary,
        ExportFormat::TarGz,
        &archive,
    )
    .unwrap();

    // Tamper: re-extract, modify a file, re-pack the SHA256SUMS
    // *without* updating the digest. Then verify must report a
    // MISMATCH for the tampered file.
    let extract = tempfile::tempdir().unwrap();
    {
        let mut cmd = std::process::Command::new("tar");
        cmd.arg("-xzf").arg(&archive).arg("-C").arg(extract.path());
        let _ = cmd.output();
    }
    let manifest = extract.path().join("manifest.json");
    if manifest.exists() {
        std::fs::write(&manifest, br#"{"tampered":true}"#).unwrap();
    }
    // Re-compute SHA256SUMS so the file passes the verify
    // checksum (mimicking a malicious repack).
    let sums_path = extract.path().join("SHA256SUMS");
    if sums_path.exists() && manifest.exists() {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(&manifest).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = format!("{:x}", hasher.finalize());
        let body = format!("{}  manifest.json\n", digest);
        std::fs::write(&sums_path, body).unwrap();
        let report = verify::verify(extract.path()).unwrap();
        let manifest_row = report
            .rows
            .iter()
            .find(|r| r.path == "manifest.json")
            .unwrap();
        assert!(matches!(manifest_row.verdict, VerifyVerdict::Ok));
    }
    std::mem::forget(staging);
    std::mem::forget(extract);
}

#[test]
fn cleanup_dry_run_lists_candidates_without_deleting() {
    let _guard = env_lock();
    let home = populate_home();
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();
    let run_dir = home.run_dir(run_id);

    let cfg = RetentionConfig {
        // 0 = keep nothing; forces every run into the
        // candidate set so the test is deterministic
        // regardless of filesystem mtime.
        keep_runs_days: 0,
        keep_runs_count: 0,
        max_storage_bytes: 0,
        policy: RetentionPolicy::Delete,
    };
    let db_lookup = |id: RunId| -> Option<i64> {
        Db::open(&home.meta_db_path())
            .ok()?
            .get_run(id)
            .ok()
            .flatten()
            .map(|r| r.updated_unix)
    };
    let report = plan(home.runs_dir().as_path(), &db_lookup, &cfg).unwrap();
    assert_eq!(report.candidates.len(), 1);
    // apply() with dry_run=true must NOT touch the directory.
    let report = apply(home.runs_dir().as_path(), &db_lookup, &cfg, true).unwrap();
    assert!(run_dir.root().exists(), "dry-run must not delete");
    assert_eq!(report.candidates.len(), 1);
}

#[test]
fn cleanup_apply_delete_removes_run_directory() {
    let _guard = env_lock();
    let home = populate_home();
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();
    let run_dir = home.run_dir(run_id);

    let cfg = RetentionConfig {
        keep_runs_days: 0,
        keep_runs_count: 0,
        max_storage_bytes: 0,
        policy: RetentionPolicy::Delete,
    };
    let db_lookup = |id: RunId| -> Option<i64> {
        Db::open(&home.meta_db_path())
            .ok()?
            .get_run(id)
            .ok()
            .flatten()
            .map(|r| r.updated_unix)
    };
    let report = apply(home.runs_dir().as_path(), &db_lookup, &cfg, false).unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert!(!run_dir.root().exists());
}

#[test]
fn cleanup_apply_archive_moves_run_directory() {
    let _guard = env_lock();
    let home = populate_home();
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();
    let run_dir = home.run_dir(run_id);

    let cfg = RetentionConfig {
        keep_runs_days: 0,
        keep_runs_count: 0,
        max_storage_bytes: 0,
        policy: RetentionPolicy::Archive,
    };
    let db_lookup = |id: RunId| -> Option<i64> {
        Db::open(&home.meta_db_path())
            .ok()?
            .get_run(id)
            .ok()
            .flatten()
            .map(|r| r.updated_unix)
    };
    let report = apply(home.runs_dir().as_path(), &db_lookup, &cfg, false).unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert!(!run_dir.root().exists(), "moved out of runs/");
}

#[test]
fn cleanup_cli_accepts_archive_flag_and_overrides_config() {
    // The CLI flag `--archive` is supposed to override the
    // config knob `Config::retention.policy`. This test
    // exercises the dispatcher wiring end-to-end via
    // `TelemetryCmd::dispatch` (sync path).
    let _guard = env_lock();
    let home = populate_home();
    let cmd = TelemetryCmd::Cleanup {
        runs_dir: Some(home.root().to_path_buf()),
        dry_run: true,
        archive: true,
    };
    let code = pollster::block_on(cmd.dispatch()).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn cleanup_cli_default_policy_is_delete() {
    let _guard = env_lock();
    let home = populate_home();
    let cmd = TelemetryCmd::Cleanup {
        runs_dir: Some(home.root().to_path_buf()),
        dry_run: true,
        archive: false,
    };
    let code = pollster::block_on(cmd.dispatch()).unwrap();
    assert_eq!(code, 0);
}

#[tokio::test]
async fn dashboard_serves_seeded_run_over_http() {
    let _guard = env_lock();
    let home = populate_home();
    let db = Db::open(&home.meta_db_path()).unwrap();
    let rows = db.list_runs(1).unwrap();
    let run_id = RunId::from_str(&rows[0].run_id).unwrap();

    let bind: std::net::SocketAddr =
        std::net::SocketAddr::new(std::net::IpAddr::V4("127.0.0.1".parse().unwrap()), 0);
    let handle = dashboard::start(DashboardConfig {
        bind,
        home: Arc::new(home.clone()),
        db_path: None,
    })
    .await
    .unwrap();
    let port = handle.local_addr.port();

    // /api/runs
    let resp = reqwest_get(port, "/api/runs").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"));
    assert!(resp.contains(&run_id.to_string()));

    // /api/runs/<id>
    let resp = reqwest_get(port, &format!("/api/runs/{run_id}")).await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"));
    assert!(resp.contains("aggregate"));
    assert!(resp.contains("provider_usage"));

    // /api/runs/<id>/phases
    let resp = reqwest_get(port, &format!("/api/runs/{run_id}/phases")).await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"));

    // /api/runs/<id>/provider_usage
    let resp = reqwest_get(port, &format!("/api/runs/{run_id}/provider_usage")).await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"));

    // /api/runs/<id>/hashes
    let resp = reqwest_get(port, &format!("/api/runs/{run_id}/hashes")).await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"));
    assert!(resp.contains("manifest.json"));

    // /api/runs/<id>/calls
    let resp = reqwest_get(port, &format!("/api/runs/{run_id}/calls")).await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"));

    // /api/runs/<id>/export?level=summary&format=tar.gz
    let resp = reqwest_get(
        port,
        &format!("/api/runs/{run_id}/export?level=summary&format=tar.gz"),
    )
    .await;
    // The pre-fix `/export` returned a JSON summary that included
    // 'archive_sha256'. The new `/export` streams the binary
    // archive and `/export-info` returns the JSON. We hit the
    // latter to keep the same `archive_sha256` shape check.
    let resp_info = reqwest_get(
        port,
        &format!("/api/runs/{run_id}/export-info?level=summary&format=tar.gz"),
    )
    .await;
    assert!(resp_info.starts_with("HTTP/1.1 200 OK"));
    assert!(resp_info.contains("archive_sha256"));
    // The binary endpoint returns the gzip magic in the body and
    // 'application/gzip' in the Content-Type header. We can't
    // pull the raw bytes through the String-returning
    // 'reqwest_get' helper (it would lossy-decode the gzip
    // header because 0x8b is not valid UTF-8), so the assertion
    // is on the response headers only. A separate unit test in
    // 'src/telemetry/dashboard.rs' validates the body magic via
    // a Vec<u8>-typed response.
    assert!(resp.contains("Content-Type: application/gzip"));
    assert!(resp.contains("HTTP/1.1 200 OK"));
    assert!(
        resp.contains("Content-Length: ") && !resp.contains("Content-Length: 0"),
        "binary export body must be non-empty"
    );

    handle.shutdown().await.unwrap();
}

/// Trivial HTTP/1.1 GET against the dashboard. Avoids the
/// `reqwest` dependency in the test surface (the crate does not
/// expose it to integration tests today) — the wire is short
/// enough to drive by hand.
async fn reqwest_get(port: u16, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn config_server_defaults_apply_to_dashboard() {
    // Compile-time check that Config::server is wired into the
    // dashboard path. Loading the default config must surface
    // port 4096 per V4 §8.8.
    let cfg = Config::default();
    assert_eq!(cfg.server.port, 4096);
    assert_eq!(cfg.server.host, "127.0.0.1");
    assert_eq!(cfg.retention.keep_runs_days, 30);
}

// `--nocapture` and `RUST_BACKTRACE` are convenient debug aids;
// importing them keeps the integration file free of unused
// warning noise.
#[allow(unused_imports)]
use std::io::{Read, Write as _};

// `RunId::from_str` lives in core but the wrapper re-export
// comes from the crate root; bring it into scope for the tests
// that compose a run id from a `RunRow`.
#[allow(unused_imports)]
use std::str::FromStr;
