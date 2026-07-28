//! Integration tests for the audit proxy and verifier.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use moagan::audit::format::{
    AuditRecord, AuditWriter, body_canonical, count_invalid_crcs, sha256_hex,
};
use moagan::audit::proxy::ProxyConfig;
use moagan::audit::verify;
use moagan::fs_layout::{MoaganHome, RunDir};
use moagan::ids::RunId;
use moagan::telemetry::CallEvent;
use tempfile::tempdir;
use wiremock::matchers::{body_bytes, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn send_http(addr: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    response
}

fn status(response: &[u8]) -> u16 {
    String::from_utf8_lossy(response)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn content_length_request(target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut request = format!("POST {target} HTTP/1.1\r\nHost: proxy.local\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut bytes = request.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn proxy_config(upstream: String, log_path: PathBuf, include_bodies: bool) -> ProxyConfig {
    ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream,
        runs_dir: log_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        run_id: None,
        include_bodies,
        upstream_timeout: Duration::from_secs(10),
        max_body_bytes: 1024 * 1024,
        refuse_loopback_forward: false,
        refuse_loopback_forward_allowed: true,
        fixed_log_path: Some(log_path),
    }
}

fn audit_text(path: &Path) -> String {
    moagan::storage::compression::read_to_string(path).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_relays_and_records_redacted_gzip_members() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-test-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-upstream-id", "request-1")
                .set_body_json(serde_json::json!({"ok": true})),
        )
        .mount(&server)
        .await;
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("external_audit.jsonl.gz");
    let handle = moagan::audit::proxy::start(proxy_config(
        format!("{}/v1", server.uri()),
        log_path.clone(),
        true,
    ))
    .await
    .unwrap();
    let body = br#"{"prompt":"key sk-cp-aaaaaaaaaaaaaaaaaaaa"}"#;
    let request = content_length_request(
        "/messages",
        &[
            ("content-type", "application/json"),
            ("x-api-key", "sk-test-123"),
        ],
        body,
    );
    let response = send_http(handle.local_addr, &request);
    assert_eq!(
        status(&response),
        200,
        "{}",
        String::from_utf8_lossy(&response)
    );
    let response_text = String::from_utf8_lossy(&response).to_ascii_lowercase();
    assert!(response_text.contains("x-upstream-id: request-1"));
    handle.shutdown().await.unwrap();

    assert_eq!(&std::fs::read(&log_path).unwrap()[..2], &[0x1f, 0x8b]);
    let text = audit_text(&log_path);
    assert_eq!(text.lines().count(), 2);
    assert!(text.contains("\"x-api-key\":\"***REDACTED***\""));
    assert!(text.contains("[REDACTED:minimax_sk_cp]"));
    assert!(!text.contains("sk-cp-aaaaaaaaaaaaaaaaaaaa"));
    assert_eq!(count_invalid_crcs(&text).0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_decodes_chunked_requests_before_forwarding() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_bytes(b"hello world"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("audit.jsonl.gz");
    let handle = moagan::audit::proxy::start(proxy_config(
        format!("{}/v1", server.uri()),
        log_path.clone(),
        true,
    ))
    .await
    .unwrap();
    let request = b"POST /messages HTTP/1.1\r\nHost: proxy\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: value\r\n\r\n";
    let response = send_http(handle.local_addr, request);
    assert_eq!(status(&response), 200);
    handle.shutdown().await.unwrap();
    let text = audit_text(&log_path);
    assert!(text.contains(&sha256_hex(b"hello world")));
    assert_eq!(count_invalid_crcs(&text).0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_preserves_real_anthropic_upstream_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let tmp = tempdir().unwrap();
    let handle = moagan::audit::proxy::start(proxy_config(
        format!("{}/anthropic/v1", server.uri()),
        tmp.path().join("audit.jsonl.gz"),
        true,
    ))
    .await
    .unwrap();
    let response = send_http(
        handle.local_addr,
        &content_length_request("/v1/messages", &[], b"{}"),
    );
    assert_eq!(
        status(&response),
        200,
        "{}",
        String::from_utf8_lossy(&response)
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_exclude_bodies_keeps_only_hashes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("secret response"))
        .mount(&server)
        .await;
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("audit.jsonl.gz");
    let handle = moagan::audit::proxy::start(proxy_config(server.uri(), log_path.clone(), false))
        .await
        .unwrap();
    let response = send_http(
        handle.local_addr,
        &content_length_request("/", &[], b"secret request"),
    );
    assert_eq!(status(&response), 200);
    handle.shutdown().await.unwrap();
    let text = audit_text(&log_path);
    assert!(!text.contains("body_canonical"));
    assert!(!text.contains("secret request"));
    assert!(!text.contains("secret response"));
    assert!(text.contains(&sha256_hex(b"secret request")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_rejects_oversized_and_absolute_requests() {
    let server = MockServer::start().await;
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("audit.jsonl.gz");
    let mut cfg = proxy_config(server.uri(), log_path.clone(), true);
    cfg.max_body_bytes = 4;
    let handle = moagan::audit::proxy::start(cfg).await.unwrap();
    let oversized = send_http(
        handle.local_addr,
        &content_length_request("/", &[], b"12345"),
    );
    assert_eq!(status(&oversized), 413);
    let absolute = send_http(
        handle.local_addr,
        b"GET http://127.0.0.1:1/private HTTP/1.1\r\nHost: proxy\r\n\r\n",
    );
    assert_eq!(status(&absolute), 400);
    handle.shutdown().await.unwrap();
    assert!(audit_text(&log_path).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_restart_appends_and_shutdown_closes_listener() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("audit.jsonl.gz");
    let first = moagan::audit::proxy::start(proxy_config(server.uri(), log_path.clone(), true))
        .await
        .unwrap();
    let first_addr = first.local_addr;
    assert_eq!(
        status(&send_http(
            first_addr,
            &content_length_request("/", &[], b"one")
        )),
        200
    );
    first.shutdown().await.unwrap();
    assert!(TcpStream::connect(first_addr).is_err());

    let second = moagan::audit::proxy::start(proxy_config(server.uri(), log_path.clone(), true))
        .await
        .unwrap();
    assert_eq!(
        status(&send_http(
            second.local_addr,
            &content_length_request("/", &[], b"two")
        )),
        200
    );
    second.shutdown().await.unwrap();
    assert_eq!(audit_text(&log_path).lines().count(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_shutdown_records_an_inflight_terminal_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_string("late"),
        )
        .mount(&server)
        .await;
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("audit.jsonl.gz");
    let handle = moagan::audit::proxy::start(proxy_config(server.uri(), log_path.clone(), true))
        .await
        .unwrap();
    let address = handle.local_addr;
    let request = content_length_request("/", &[], b"in-flight");
    let client = tokio::task::spawn_blocking(move || send_http(address, &request));
    let mut request_logged = false;
    for _ in 0..100 {
        if moagan::storage::compression::read_to_string(&log_path)
            .is_ok_and(|text| text.contains("\"event\":\"request\""))
        {
            request_logged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(request_logged);
    handle.shutdown().await.unwrap();
    let response = client.await.unwrap();
    assert_eq!(status(&response), 502);
    let text = audit_text(&log_path);
    assert!(text.contains("\"event\":\"upstream_error\""));
    assert_eq!(text.lines().count(), 2);
    assert_eq!(count_invalid_crcs(&text).0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_rejects_a_self_referential_upstream() {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let tmp = tempdir().unwrap();
    let mut cfg = proxy_config(
        format!("http://{address}/v1"),
        tmp.path().join("audit.jsonl.gz"),
        true,
    );
    cfg.listen = address;
    let error = moagan::audit::proxy::start(cfg).await.unwrap_err();
    assert!(error.to_string().contains("proxy itself"));
}

fn create_run<'a>(home: &'a MoaganHome) -> RunDir<'a> {
    let run = home.run_dir(RunId::new());
    run.ensure().unwrap();
    run
}

fn external_record(event: &str, id: &str, ts: f64, hash: &str) -> AuditRecord {
    AuditRecord {
        ts,
        event: event.into(),
        id: id.into(),
        method: (event == "request").then(|| "POST".into()),
        url: (event == "request").then(|| "http://upstream/messages".into()),
        status: (event == "response").then_some(200),
        headers: Default::default(),
        body_canonical: None,
        body_sha256: hash.into(),
        body_size: 2,
        elapsed_ms: (event != "request").then_some(1),
        crc32: String::new(),
        error: None,
    }
}

fn write_external(run: &RunDir<'_>, records: &mut [AuditRecord]) {
    let mut writer = AuditWriter::create(&run.external_audit_path()).unwrap();
    for record in records {
        writer.write_record(record).unwrap();
    }
    writer.flush_gz().unwrap();
}

fn call_event(id: &str, ts: i64, hash: Option<&str>, cache_hit: bool) -> CallEvent {
    CallEvent {
        run_id: "run".into(),
        call_id: id.into(),
        phase: "judge".into(),
        role: "judge".into(),
        provider: "minimax".into(),
        model: "MiniMax-M3".into(),
        cache_key: "cache".into(),
        body_sha256: hash.map(str::to_owned),
        cache_hit,
        http_status: Some(200),
        input_tokens: 1,
        output_tokens: 1,
        cache_read: 0,
        cache_creation: 0,
        started_unix: ts,
        ended_unix: ts + 1,
        error: None,
        status: Some("ok".into()),
    }
}

fn write_calls(run: &RunDir<'_>, events: &[CallEvent]) -> PathBuf {
    let path = run.telemetry().join("calls.jsonl.gz");
    let mut writer = moagan::storage::compression::open_gz_append(&path).unwrap();
    for event in events {
        writeln!(writer, "{}", serde_json::to_string(event).unwrap()).unwrap();
        writer.flush().unwrap();
    }
    path
}

#[test]
fn verify_exact_hash_match_is_exit_zero() {
    let tmp = tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();
    let run = create_run(&home);
    let hash = sha256_hex(b"{}");
    write_external(
        &run,
        &mut [
            external_record("request", "pair", 100.25, &hash),
            external_record("response", "pair", 100.5, &sha256_hex(b"ok")),
        ],
    );
    let calls = write_calls(&run, &[call_event("call", 100, Some(&hash), false)]);
    let report = verify::verify(&run, &calls).unwrap();
    assert_eq!(report.match_count, 1);
    assert_eq!(report.summary(), "ok");
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn verify_accepts_a_recorded_upstream_error_as_terminal() {
    let tmp = tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();
    let run = create_run(&home);
    let hash = sha256_hex(b"request");
    write_external(
        &run,
        &mut [
            external_record("request", "pair", 100.0, &hash),
            external_record("upstream_error", "pair", 100.2, &sha256_hex(b"")),
        ],
    );
    let calls = write_calls(&run, &[call_event("call", 100, Some(&hash), false)]);
    let report = verify::verify(&run, &calls).unwrap();
    assert_eq!(report.match_count, 1);
    assert_eq!(report.orphan_request_count, 0);
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn verify_detects_body_mismatch_and_orphan() {
    let tmp = tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();
    let run = create_run(&home);
    let external_hash = sha256_hex(b"external");
    write_external(
        &run,
        &mut [
            external_record("request", "pair", 100.0, &external_hash),
            external_record("response", "pair", 100.1, &sha256_hex(b"ok")),
            external_record("request", "orphan", 101.0, &external_hash),
        ],
    );
    let calls = write_calls(
        &run,
        &[call_event(
            "call",
            100,
            Some(&sha256_hex(b"internal")),
            false,
        )],
    );
    let report = verify::verify(&run, &calls).unwrap();
    assert_eq!(report.body_mismatch_count, 1);
    assert_eq!(report.orphan_request_count, 1);
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn verify_ignores_cache_hits_without_http_traffic() {
    let tmp = tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();
    let run = create_run(&home);
    std::fs::File::create(run.external_audit_path()).unwrap();
    let calls = write_calls(&run, &[call_event("cached", 100, None, true)]);
    let report = verify::verify(&run, &calls).unwrap();
    assert_eq!(report.summary(), "ok");
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn verify_crc_corruption_and_missing_inputs_are_exit_two() {
    let tmp = tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();
    let run = create_run(&home);
    let missing = verify::verify(&run, &run.telemetry().join("calls.jsonl.gz")).unwrap();
    assert!(missing.audit_file_missing);
    assert_eq!(missing.exit_code(), 2);

    let mut bad = external_record("request", "bad", 100.0, &sha256_hex(b"bad"));
    bad.crc32 = "00000000".into();
    let mut line = serde_json::to_vec(&bad).unwrap();
    line.push(b'\n');
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&line).unwrap();
    let member = encoder.finish().unwrap();
    std::fs::write(run.external_audit_path(), member).unwrap();
    let calls = write_calls(&run, &[]);
    let corrupt = verify::verify(&run, &calls).unwrap();
    assert!(corrupt.crc_invalid_count > 0);
    assert_eq!(corrupt.exit_code(), 2);

    let truncated_run = create_run(&home);
    std::fs::write(truncated_run.external_audit_path(), [0x1f, 0x8b, 0x08]).unwrap();
    let truncated_calls = write_calls(&truncated_run, &[]);
    let truncated = verify::verify(&truncated_run, &truncated_calls).unwrap();
    assert!(truncated.crc_invalid_count > 0);
    assert_eq!(truncated.exit_code(), 2);
}

#[test]
fn body_canonical_still_orders_json_keys() {
    assert_eq!(body_canonical(br#"{"b":2,"a":1}"#), r#"{"a":1,"b":2}"#);
}
