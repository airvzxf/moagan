//! Integration tests for the `moagan audit` family: the sidecar
//! proxy and the verifier. Uses `wiremock` for the upstream leg so
//! the tests stay local and deterministic.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use moagan::audit::format::{
    AuditRecord, AuditWriter, body_canonical, count_invalid_crcs, sha256_hex,
};
use moagan::audit::proxy::ProxyConfig;
use moagan::audit::verify::{self, VerifyReport};
use moagan::fs_layout::{MoaganHome, RunDir};
use moagan::ids::RunId;
use tempfile::tempdir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn send_http(addr: std::net::SocketAddr, req: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(req).unwrap();
    s.flush().unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    out
}

fn parse_status(body: &[u8]) -> u16 {
    let line = std::str::from_utf8(body)
        .unwrap()
        .lines()
        .next()
        .unwrap_or("");
    let code = line.split_whitespace().nth(1).unwrap_or("0");
    code.parse().unwrap_or(0)
}

fn request_line(
    method: &str,
    url: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut s = format!("{method} {url} HTTP/1.1\r\nHost: {host}\r\n");
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut out = s.into_bytes();
    out.extend_from_slice(body);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_proxy_relays_request_with_kept_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-test-123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&server)
        .await;

    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("external_audit.jsonl.gz");
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: format!("{}/v1", server.uri()),
        log_path: log_path.clone(),
        include_bodies: true,
        upstream_timeout: Duration::from_secs(10),
        max_body_bytes: 1024 * 1024,
        refuse_loopback_forward: false,
        refuse_loopback_forward_allowed: false,
    };
    let handle = moagan::audit::proxy::start(cfg).await.unwrap();
    let port = handle.local_addr.port();

    let body = br#"{"hello":"world"}"#;
    let req = request_line(
        "POST",
        "/messages",
        "upstream.local",
        &[
            ("content-type", "application/json"),
            ("x-api-key", "sk-test-123"),
        ],
        body,
    );
    let resp = send_http(std::net::SocketAddr::from(([127, 0, 0, 1], port)), &req);
    assert_eq!(
        parse_status(&resp),
        200,
        "got {}",
        String::from_utf8_lossy(&resp)
    );
    let body_str = std::str::from_utf8(body).unwrap();
    let resp_str = std::str::from_utf8(&resp).unwrap();
    assert!(
        resp_str.contains("\"ok\":true"),
        "response {resp_str:?} missing upstream body"
    );
    assert!(
        !resp_str.contains(body_str),
        "request body must NOT be echoed"
    );
    handle.shutdown().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let text = std::fs::read_to_string(&log_path).unwrap();
    assert!(text.contains("\"event\":\"request\""));
    assert!(text.contains("\"event\":\"response\""));
    assert!(text.contains("\"status\":200"));
    let (invalid, bad) = count_invalid_crcs(&text);
    assert_eq!(invalid, 0, "bad lines: {bad:?}");
    assert!(text.contains("\"x-api-key\":\"***REDACTED***\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_proxy_supports_chunked_encoding() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("audit.jsonl.gz");
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: format!("{}/v1", server.uri()),
        log_path: log_path.clone(),
        include_bodies: true,
        upstream_timeout: Duration::from_secs(10),
        max_body_bytes: 1024 * 1024,
        refuse_loopback_forward: false,
        refuse_loopback_forward_allowed: false,
    };
    let handle = moagan::audit::proxy::start(cfg).await.unwrap();
    let port = handle.local_addr.port();
    let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
    let mut req = Vec::new();
    req.extend_from_slice(
        b"POST /messages HTTP/1.1\r\nHost: upstream\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    req.extend_from_slice(chunked);
    let resp = send_http(std::net::SocketAddr::from(([127, 0, 0, 1], port)), &req);
    assert_eq!(
        parse_status(&resp),
        200,
        "got {}",
        String::from_utf8_lossy(&resp)
    );
    handle.shutdown().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let text = std::fs::read_to_string(&log_path).unwrap();
    assert!(text.contains("\"event\":\"response\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_proxy_refuses_loopback_upstream() {
    let tmp = tempdir().unwrap();
    let log_path = tmp.path().join("audit.jsonl.gz");
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: "http://127.0.0.1:9/v1".into(),
        log_path,
        include_bodies: true,
        upstream_timeout: Duration::from_secs(10),
        max_body_bytes: 1024,
        refuse_loopback_forward: true,
        refuse_loopback_forward_allowed: false,
    };
    let err = moagan::audit::proxy::start(cfg).await.unwrap_err();
    assert!(err.to_string().contains("refusing to forward to loopback"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_verify_perfect_match() {
    let tmp = tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir: RunDir<'_> = home.run_dir(run_id);
    run_dir.ensure().unwrap();

    let audit_path = run_dir.external_audit_path();

    let arc = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    {
        let mut w = AuditWriter::from_mutexed(std::sync::Arc::clone(&arc));
        let body: &[u8] = b"{\"model\":\"x\"}";
        let req = AuditRecord {
            ts: 100.0,
            event: "request".into(),
            id: "id-1".into(),
            method: Some("POST".into()),
            url: Some("http://upstream".into()),
            status: None,
            headers: Default::default(),
            body_canonical: Some(body_canonical(body)),
            body_sha256: sha256_hex(body),
            body_size: body.len() as u64,
            elapsed_ms: None,
            crc32: String::new(),
            error: None,
        };
        let resp = AuditRecord {
            ts: 101.0,
            event: "response".into(),
            id: "id-1".into(),
            method: None,
            url: None,
            status: Some(200),
            headers: Default::default(),
            body_canonical: Some("{\"ok\":1}".into()),
            body_sha256: sha256_hex(b"{\"ok\":1}"),
            body_size: 7,
            elapsed_ms: Some(1000),
            crc32: String::new(),
            error: None,
        };
        let mut r1 = req;
        let mut r2 = resp;
        w.write_record(&mut r1).unwrap();
        w.write_record(&mut r2).unwrap();
        w.flush_gz().unwrap();
    }
    let bytes = arc.lock().unwrap().clone();
    std::fs::write(&audit_path, &bytes).unwrap();
    let body_str = body_canonical(b"{\"model\":\"x\"}");
    let call_line = format!(
        "{{\"body_canonical\":\"{}\",\"started_unix\":100}}\n",
        body_str.replace('\\', "\\\\").replace('"', "\\\"")
    );
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let gz_path = run_dir.telemetry().join("calls.jsonl.gz");
    let gz_file = std::fs::File::create(&gz_path).unwrap();
    let mut gz = GzEncoder::new(gz_file, Compression::default());
    std::io::Write::write_all(&mut gz, call_line.as_bytes()).unwrap();
    std::io::Write::write_all(&mut gz, b"\n").unwrap();
    gz.finish().unwrap();
    let report = verify::verify(&run_dir, &gz_path).unwrap();
    assert_eq!(report.match_count, 1);
    assert_eq!(report.body_mismatch_count, 0);
    assert!(report.summary() == "ok" || report.summary() == "mismatch");
    assert_eq!(report.exit_code(), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_verify_crc_invalid() {
    let tmp = tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    let audit_path = run_dir.external_audit_path();
    let calls_path = run_dir.telemetry().join("calls.jsonl");
    {
        let mut w = AuditWriter::create(&audit_path).unwrap();
        let mut r = AuditRecord {
            ts: 100.0,
            event: "request".into(),
            id: "id-1".into(),
            method: Some("POST".into()),
            url: Some("http://upstream".into()),
            status: None,
            headers: Default::default(),
            body_canonical: None,
            body_sha256: sha256_hex(b"hello"),
            body_size: 5,
            elapsed_ms: None,
            crc32: String::new(),
            error: None,
        };
        w.write_record(&mut r).unwrap();
    }
    let mut raw = std::fs::read(&audit_path).unwrap();
    raw.extend_from_slice(b"{\"crc32\":\"00000000\"}\n");
    std::fs::write(&audit_path, &raw).unwrap();
    let report = verify::verify(&run_dir, &calls_path).unwrap();
    assert_eq!(report.crc_invalid_count, 1);
    assert_eq!(report.exit_code(), 2);
}

#[test]
fn audit_report_default_is_ok() {
    let r = VerifyReport::default();
    assert_eq!(r.summary(), "ok");
    assert_eq!(r.exit_code(), 0);
}
