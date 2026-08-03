//! HTTP/1.1 forwarder used by `moagan audit proxy`.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::redirect::Policy;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::redact::{RedactPolicy, Surface, apply};

use super::format::{AuditRecord, AuditWriter, body_canonical, redact_header, sha256_hex};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 8 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Configures the sidecar.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Address to bind on.
    pub listen: SocketAddr,
    /// Resolved upstream base URL.
    pub upstream: String,
    /// Root directory containing `.runs/`.
    pub runs_dir: PathBuf,
    /// Optional fixed run id.
    pub run_id: Option<RunId>,
    /// Whether canonical bodies are persisted.
    pub include_bodies: bool,
    /// Per-request upstream timeout.
    pub upstream_timeout: Duration,
    /// Hard body-size cap.
    pub max_body_bytes: usize,
    /// Refuse loopback upstreams unless explicitly allowed.
    pub refuse_loopback_forward: bool,
    /// Explicit loopback-upstream opt-in.
    pub refuse_loopback_forward_allowed: bool,
    /// Optional fixed log path for tests.
    pub fixed_log_path: Option<PathBuf>,
}

impl ProxyConfig {
    /// Fill zero-valued limits with defaults.
    pub fn with_defaults(mut self) -> Self {
        if self.max_body_bytes == 0 {
            self.max_body_bytes = 32 * 1024 * 1024;
        }
        if self.upstream_timeout.is_zero() {
            self.upstream_timeout = Duration::from_secs(180);
        }
        self
    }
}

/// Bound proxy and its cooperative shutdown handle.
pub struct ProxyHandle {
    /// Address selected by the listener.
    pub local_addr: SocketAddr,
    shutdown: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
}

impl std::fmt::Debug for ProxyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyHandle")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl ProxyHandle {
    /// Stop accepting connections and drain active handlers.
    pub async fn shutdown(mut self) -> Result<()> {
        self.shutdown.cancel();
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await
            .map_err(|e| Error::InvalidState(format!("audit proxy task failed: {e}")))?
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct AuditSink {
    writers: HashMap<PathBuf, AuditWriter>,
}

impl AuditSink {
    fn new() -> Self {
        Self {
            writers: HashMap::new(),
        }
    }

    fn ensure(&mut self, path: &Path) -> Result<()> {
        if self.writers.contains_key(path) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let writer = AuditWriter::append(path)?;
        self.writers.insert(path.to_path_buf(), writer);
        Ok(())
    }

    fn write(&mut self, path: &Path, record: &mut AuditRecord) -> Result<()> {
        self.ensure(path)?;
        let writer = self
            .writers
            .get_mut(path)
            .ok_or_else(|| Error::InvalidState("audit writer was not installed".into()))?;
        writer.write_record(record)?;
        Ok(())
    }

    fn flush_all(&mut self) -> Result<()> {
        for writer in self.writers.values_mut() {
            writer.flush_gz()?;
        }
        Ok(())
    }
}

/// Bind and start the sidecar.
pub async fn start(cfg: ProxyConfig) -> Result<ProxyHandle> {
    let cfg = Arc::new(cfg.with_defaults());
    validate_upstream(&cfg.upstream)?;
    let listener = TcpListener::bind(cfg.listen).await?;
    let local_addr = listener.local_addr()?;
    validate_forward_target(&cfg, local_addr)?;

    let client = reqwest::Client::builder()
        .timeout(cfg.upstream_timeout)
        .connect_timeout(Duration::from_secs(15))
        .redirect(Policy::none())
        .no_gzip()
        .build()
        .map_err(|e| Error::Provider(format!("build audit HTTP client: {e}")))?;
    let sink = Arc::new(Mutex::new(AuditSink::new()));
    if let Some(path) = resolve_log_path(&cfg)? {
        sink.lock().await.ensure(&path)?;
    }
    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(serve(listener, cfg, client, sink, task_shutdown));
    Ok(ProxyHandle {
        local_addr,
        shutdown,
        task: Some(task),
    })
}

async fn serve(
    listener: TcpListener,
    cfg: Arc<ProxyConfig>,
    client: reqwest::Client,
    sink: Arc<Mutex<AuditSink>>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut handlers = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let cfg = Arc::clone(&cfg);
                let client = client.clone();
                let sink = Arc::clone(&sink);
                let handler_shutdown = shutdown.clone();
                handlers.spawn(async move {
                    handle_connection(stream, cfg, client, sink, handler_shutdown).await
                });
            }
            joined = handlers.join_next(), if !handlers.is_empty() => {
                if let Some(result) = joined {
                    report_handler_result(result);
                }
            }
        }
    }

    let drain = async {
        while let Some(result) = handlers.join_next().await {
            report_handler_result(result);
        }
    };
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, drain).await.is_err() {
        handlers.abort_all();
        while handlers.join_next().await.is_some() {}
    }
    sink.lock().await.flush_all()?;
    Ok(())
}

fn report_handler_result(result: std::result::Result<Result<()>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("warn: audit proxy connection failed: {e}"),
        Err(e) if e.is_cancelled() => {}
        Err(e) => eprintln!("warn: audit proxy handler failed: {e}"),
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    cfg: Arc<ProxyConfig>,
    client: reqwest::Client,
    sink: Arc<Mutex<AuditSink>>,
    shutdown: CancellationToken,
) -> Result<()> {
    let parsed = tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        result = tokio::time::timeout(IO_TIMEOUT, read_request(&mut stream, cfg.max_body_bytes)) => {
            match result {
                Ok(Ok(request)) => request,
                Ok(Err(e)) => {
                    write_error(&mut stream, e.status, &e.message).await?;
                    return Ok(());
                }
                Err(_) => {
                    write_error(&mut stream, 408, "request timeout").await?;
                    return Ok(());
                }
            }
        }
    };

    let Some(log_path) = resolve_log_path_blocking(&cfg)? else {
        write_error(&mut stream, 503, "no active run").await?;
        return Ok(());
    };
    let policy = RedactPolicy::default();
    let id = uuid::Uuid::now_v7().to_string();
    let started = Instant::now();
    let request_ts = unix_now();
    let upstream_url = join_upstream(&cfg.upstream, &parsed.target, None)?;
    let request_sha = sha256_hex(&parsed.body);
    let request_body = canonical_redacted_body(&policy, &parsed.body, cfg.include_bodies)?;
    let request_headers = redacted_headers(&policy, &parsed.headers)?;
    let logged_url = apply(&policy, Surface::Telemetry, &upstream_url)?.into_owned();
    let mut request_record = AuditRecord {
        ts: request_ts,
        event: "request".into(),
        id: id.clone(),
        method: Some(parsed.method.as_str().to_owned()),
        url: Some(logged_url),
        status: None,
        headers: request_headers,
        body_canonical: request_body,
        body_sha256: request_sha,
        body_size: parsed.body.len() as u64,
        elapsed_ms: None,
        crc32: String::new(),
        error: None,
    };
    sink.lock().await.write(&log_path, &mut request_record)?;

    let mut forwarded_headers = reqwest::header::HeaderMap::new();
    for (name, value) in &parsed.headers {
        if is_hop_by_hop_request_header(name) {
            continue;
        }
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_str(value) else {
            continue;
        };
        forwarded_headers.append(name, value);
    }
    let request = client
        .request(parsed.method, &upstream_url)
        .headers(forwarded_headers)
        .body(parsed.body);
    let response = tokio::select! {
        _ = shutdown.cancelled() => {
            record_upstream_error(
                &sink,
                &log_path,
                &id,
                started.elapsed(),
                "proxy shutting down",
                &policy,
            ).await?;
            write_error(&mut stream, 502, "proxy shutting down").await?;
            return Ok(());
        }
        response = request.send() => response,
    };
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            record_upstream_error(
                &sink,
                &log_path,
                &id,
                started.elapsed(),
                &e.to_string(),
                &policy,
            )
            .await?;
            write_error(&mut stream, 502, "upstream error").await?;
            return Ok(());
        }
    };

    let status = response.status();
    let response_headers = response.headers().clone();
    let response_body = match read_response_body(response, cfg.max_body_bytes).await {
        Ok(body) => body,
        Err(e) => {
            record_upstream_error(&sink, &log_path, &id, started.elapsed(), &e, &policy).await?;
            write_error(&mut stream, 502, "upstream body error").await?;
            return Ok(());
        }
    };
    let response_pairs = response_headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
        })
        .collect::<Vec<_>>();
    let response_audit_headers = redacted_headers(&policy, &response_pairs)?;
    let response_canonical = canonical_redacted_body(&policy, &response_body, cfg.include_bodies)?;
    let mut response_record = AuditRecord {
        ts: unix_now(),
        event: "response".into(),
        id,
        method: None,
        url: None,
        status: Some(status.as_u16()),
        headers: response_audit_headers,
        body_canonical: response_canonical,
        body_sha256: sha256_hex(&response_body),
        body_size: response_body.len() as u64,
        elapsed_ms: Some(duration_ms(started.elapsed())),
        crc32: String::new(),
        error: None,
    };
    sink.lock().await.write(&log_path, &mut response_record)?;

    write_response(
        &mut stream,
        &parsed.version,
        status,
        &response_headers,
        &response_body,
    )
    .await
}

async fn record_upstream_error(
    sink: &Arc<Mutex<AuditSink>>,
    path: &Path,
    id: &str,
    elapsed: Duration,
    error: &str,
    policy: &RedactPolicy,
) -> Result<()> {
    let error = apply(policy, Surface::Telemetry, error)?.into_owned();
    let mut record = AuditRecord {
        ts: unix_now(),
        event: "upstream_error".into(),
        id: id.to_owned(),
        method: None,
        url: None,
        status: None,
        headers: BTreeMap::new(),
        body_canonical: None,
        body_sha256: sha256_hex(b""),
        body_size: 0,
        elapsed_ms: Some(duration_ms(elapsed)),
        crc32: String::new(),
        error: Some(error),
    };
    sink.lock().await.write(path, &mut record)
}

struct ParsedRequest {
    method: reqwest::Method,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct RequestReadError {
    status: u16,
    message: String,
}

impl RequestReadError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(400, format!("malformed request: {error}"))
    }
}

async fn read_request(
    stream: &mut TcpStream,
    cap: usize,
) -> std::result::Result<ParsedRequest, RequestReadError> {
    let mut reader = BufReader::new(stream);
    let mut total = 0usize;
    let request_line = read_bounded_line(&mut reader, &mut total).await?;
    let fields = request_line
        .trim_end_matches(['\r', '\n'])
        .split_whitespace()
        .collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err(RequestReadError::new(400, "malformed request line"));
    }
    let method = reqwest::Method::from_bytes(fields[0].as_bytes())
        .map_err(|_| RequestReadError::new(400, "invalid HTTP method"))?;
    let target = fields[1].to_owned();
    if !target.starts_with('/') || target.starts_with("//") {
        return Err(RequestReadError::new(
            400,
            "absolute request targets are not allowed",
        ));
    }
    let version = fields[2].to_owned();
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(RequestReadError::new(505, "unsupported HTTP version"));
    }

    let mut headers = Vec::new();
    let mut content_length = None;
    let mut transfer_encoding = None;
    loop {
        let line = read_bounded_line(&mut reader, &mut total).await?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        if line.is_empty() {
            return Err(RequestReadError::new(400, "unexpected end of headers"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(RequestReadError::new(400, "malformed header"));
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name.is_empty() {
            return Err(RequestReadError::new(400, "empty header name"));
        }
        if name == "content-length" {
            let parsed = value
                .parse::<usize>()
                .map_err(|_| RequestReadError::new(400, "invalid content-length"))?;
            if content_length.is_some_and(|prior| prior != parsed) {
                return Err(RequestReadError::new(
                    400,
                    "conflicting content-length headers",
                ));
            }
            content_length = Some(parsed);
        }
        if name == "transfer-encoding" {
            transfer_encoding = Some(value.to_ascii_lowercase());
        }
        headers.push((name, value));
    }
    if content_length.is_some() && transfer_encoding.is_some() {
        return Err(RequestReadError::new(400, "ambiguous request framing"));
    }
    let body = match transfer_encoding.as_deref() {
        Some("chunked") => read_chunked_body(&mut reader, cap, &mut total).await?,
        Some(_) => return Err(RequestReadError::new(501, "unsupported transfer encoding")),
        None => match content_length {
            Some(length) => read_n_body(&mut reader, length, cap).await?,
            None => Vec::new(),
        },
    };
    Ok(ParsedRequest {
        method,
        target,
        version,
        headers,
        body,
    })
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    total: &mut usize,
) -> std::result::Result<String, RequestReadError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(RequestReadError::io)?;
        if available.is_empty() {
            break;
        }
        let count = memchr::memchr(b'\n', available).map_or(available.len(), |index| index + 1);
        let next_len = bytes
            .len()
            .checked_add(count)
            .ok_or_else(|| RequestReadError::new(431, "headers too large"))?;
        if next_len > MAX_LINE_BYTES {
            return Err(RequestReadError::new(431, "headers too large"));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    *total = total
        .checked_add(bytes.len())
        .ok_or_else(|| RequestReadError::new(431, "headers too large"))?;
    if *total > MAX_HEADER_BYTES {
        return Err(RequestReadError::new(431, "headers too large"));
    }
    String::from_utf8(bytes).map_err(|_| RequestReadError::new(400, "headers are not UTF-8"))
}

async fn read_n_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    length: usize,
    cap: usize,
) -> std::result::Result<Vec<u8>, RequestReadError> {
    if length > cap {
        return Err(RequestReadError::new(413, "payload too large"));
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(RequestReadError::io)?;
    Ok(body)
}

async fn read_chunked_body<R: AsyncBufRead + AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
    header_total: &mut usize,
) -> std::result::Result<Vec<u8>, RequestReadError> {
    let mut body = Vec::new();
    loop {
        let line = read_bounded_line(reader, header_total).await?;
        let size = line
            .trim_end_matches(['\r', '\n'])
            .split(';')
            .next()
            .and_then(|value| usize::from_str_radix(value.trim(), 16).ok())
            .ok_or_else(|| RequestReadError::new(400, "invalid chunk size"))?;
        if size == 0 {
            loop {
                let trailer = read_bounded_line(reader, header_total).await?;
                if trailer == "\r\n" || trailer == "\n" {
                    break;
                }
                if trailer.is_empty() || !trailer.contains(':') {
                    return Err(RequestReadError::new(400, "malformed chunk trailer"));
                }
            }
            break;
        }
        let next_len = body
            .len()
            .checked_add(size)
            .ok_or_else(|| RequestReadError::new(413, "payload too large"))?;
        if next_len > cap {
            return Err(RequestReadError::new(413, "payload too large"));
        }
        let start = body.len();
        body.resize(next_len, 0);
        reader
            .read_exact(&mut body[start..])
            .await
            .map_err(RequestReadError::io)?;
        let mut crlf = [0; 2];
        reader
            .read_exact(&mut crlf)
            .await
            .map_err(RequestReadError::io)?;
        if crlf != *b"\r\n" {
            return Err(RequestReadError::new(400, "malformed chunk terminator"));
        }
    }
    Ok(body)
}

async fn read_response_body(
    response: reqwest::Response,
    cap: usize,
) -> std::result::Result<Vec<u8>, String> {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| "upstream body exceeds size limit".to_owned())?;
        if next_len > cap {
            return Err("upstream body exceeds size limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn write_response(
    stream: &mut TcpStream,
    version: &str,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> Result<()> {
    let reason = status.canonical_reason().unwrap_or("");
    stream
        .write_all(format!("{version} {} {reason}\r\n", status.as_u16()).as_bytes())
        .await?;
    for (name, value) in headers {
        if is_hop_by_hop_response_header(name.as_str()) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            stream
                .write_all(format!("{}: {value}\r\n", name.as_str()).as_bytes())
                .await?;
        }
    }
    stream
        .write_all(
            format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

async fn write_error(stream: &mut TcpStream, status: u16, message: &str) -> Result<()> {
    let status_code =
        reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    let reason = status_code.canonical_reason().unwrap_or("");
    let body = format!("{status} {message}\n");
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn canonical_redacted_body(
    policy: &RedactPolicy,
    bytes: &[u8],
    include: bool,
) -> Result<Option<String>> {
    if !include {
        return Ok(None);
    }
    let canonical = body_canonical(bytes);
    Ok(Some(
        apply(policy, Surface::Telemetry, &canonical)?.into_owned(),
    ))
}

fn redacted_headers(
    policy: &RedactPolicy,
    headers: &[(String, String)],
) -> Result<BTreeMap<String, String>> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = redact_header(name, value);
            let value = apply(policy, Surface::Telemetry, &value)?.into_owned();
            Ok((name.clone(), value))
        })
        .collect()
}

fn is_hop_by_hop_request_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "connection"
            | "accept-encoding"
            | "transfer-encoding"
            | "proxy-connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn is_hop_by_hop_response_header(name: &str) -> bool {
    matches!(
        name,
        "content-length"
            | "connection"
            | "transfer-encoding"
            | "proxy-connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn resolve_log_path(cfg: &ProxyConfig) -> Result<Option<PathBuf>> {
    if let Some(path) = &cfg.fixed_log_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        return Ok(Some(path.clone()));
    }
    let home = MoaganHome::at(cfg.runs_dir.clone());
    if let Some(run_id) = cfg.run_id {
        let run_dir = home.run_dir(run_id);
        run_dir.ensure()?;
        return Ok(Some(run_dir.external_audit_path()));
    }
    let runs_root = cfg.runs_dir.join(".runs");
    std::fs::create_dir_all(&runs_root)?;
    // `start_proxy` runs this on the startup banner; the
    // connect loop runs it on every request. We split the two
    // entry points so the startup banner does not block: the
    // non-blocking form returns Ok(None) when no run exists,
    // and the request form (`resolve_log_path_blocking`)
    // polls with backoff so the first LLM call from a freshly
    // spawned run lands instead of triggering a 503.
    if let Some(id) = pick_latest_run(&runs_root)? {
        let run_dir = home.run_dir(id);
        run_dir.ensure()?;
        return Ok(Some(run_dir.external_audit_path()));
    }
    Ok(None)
}

/// Blocking variant for the per-request path. Waits up to 10s
/// for a run dir to appear with exponential backoff (50ms → 1s).
/// Returns `Ok(None)` after the deadline so the connection
/// handler surfaces 503 'no active run'.
fn resolve_log_path_blocking(cfg: &ProxyConfig) -> Result<Option<PathBuf>> {
    if let Some(path) = &cfg.fixed_log_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        return Ok(Some(path.clone()));
    }
    let home = MoaganHome::at(cfg.runs_dir.clone());
    if let Some(run_id) = cfg.run_id {
        let run_dir = home.run_dir(run_id);
        run_dir.ensure()?;
        return Ok(Some(run_dir.external_audit_path()));
    }
    let runs_root = cfg.runs_dir.join(".runs");
    std::fs::create_dir_all(&runs_root)?;
    // Backoff schedule starts at 5ms (not 50ms) because the
    // typical e2e tests + CI runs have moagan create the run dir
    // and immediately fire the first LLM call within ~5-10ms. A
    // 50ms initial window was empirically the cause of
    // `audit_e2e_deep_run_has_exact_external_coverage` flaking
    // 2/15 times: 2-4 LLM calls from `intake` / `route` arrived at
    // the proxy before the first 50ms poll, the proxy answered
    // 503 `no active run`, and moagan retried the call after the
    // proxy had already cached the run_id. The 503 path leaves a
    // record in `calls.jsonl` with `cache_hit=false` but no
    // matching entry in the audit log, so `audit verify` reports
    // `unmatched_internal_count > 0`. Starting at 5ms and ramping
    // up (5, 10, 20, 50, 100, 200, 400, 800, 1000, 1000, ...)
    // catches the run dir inside the first tick on every CI
    // machine tested so far.
    let mut wait_ms: u64 = 5;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let run_id = loop {
        if let Some(id) = pick_latest_run(&runs_root)? {
            break id;
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        wait_ms = (wait_ms * 2).min(1000);
    };
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    Ok(Some(run_dir.external_audit_path()))
}

fn pick_latest_run(runs_root: &Path) -> Result<Option<RunId>> {
    let mut latest = None;
    for entry in std::fs::read_dir(runs_root)? {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(run_id) = name.parse::<RunId>() else {
            continue;
        };
        if latest.is_none_or(|current| run_id > current) {
            latest = Some(run_id);
        }
    }
    Ok(latest)
}

fn validate_upstream(upstream: &str) -> Result<()> {
    let url = reqwest::Url::parse(upstream)
        .map_err(|e| Error::InvalidArgs(format!("invalid upstream URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::InvalidArgs(
            "upstream must be an absolute http(s) URL".into(),
        ));
    }
    if url.fragment().is_some() {
        return Err(Error::InvalidArgs(
            "upstream URL must not contain a fragment".into(),
        ));
    }
    Ok(())
}

fn validate_forward_target(cfg: &ProxyConfig, local_addr: SocketAddr) -> Result<()> {
    let url = reqwest::Url::parse(&cfg.upstream)
        .map_err(|e| Error::InvalidArgs(format!("invalid upstream URL: {e}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| Error::InvalidArgs("upstream URL has no host".into()))?;
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if cfg.refuse_loopback_forward && !cfg.refuse_loopback_forward_allowed && is_loopback {
        return Err(Error::InvalidArgs(format!(
            "refusing to forward to loopback upstream {}",
            cfg.upstream
        )));
    }
    let port = url.port_or_known_default();
    if is_loopback
        && port == Some(local_addr.port())
        && (local_addr.ip().is_loopback() || local_addr.ip().is_unspecified())
    {
        return Err(Error::InvalidArgs(
            "upstream resolves to the audit proxy itself".into(),
        ));
    }
    Ok(())
}

fn join_upstream(base: &str, target: &str, _host_hint: Option<&str>) -> Result<String> {
    if !target.starts_with('/') || target.starts_with("//") {
        return Err(Error::InvalidArgs(
            "absolute request targets are not allowed".into(),
        ));
    }
    let mut url = reqwest::Url::parse(base)
        .map_err(|e| Error::InvalidArgs(format!("invalid upstream URL: {e}")))?;
    let (target_path, query) = target
        .split_once('?')
        .map_or((target, None), |(path, query)| (path, Some(query)));
    if target_path != "/" {
        let base_segments = url
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let target_segments = target_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        let max_overlap = base_segments.len().min(target_segments.len());
        let overlap = (0..=max_overlap)
            .rev()
            .find(|count| {
                base_segments[base_segments.len().saturating_sub(*count)..]
                    == target_segments[..*count]
            })
            .unwrap_or(0);
        let mut combined = base_segments;
        combined.extend_from_slice(&target_segments[overlap..]);
        url.set_path(&format!("/{}", combined.join("/")));
    }
    url.set_query(query);
    url.set_fragment(None);
    Ok(url.into())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn unix_now() -> f64 {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let micros = (duration.as_secs_f64() * 1_000_000.0).round();
    micros / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_upstream_preserves_anthropic_prefix() {
        assert_eq!(
            join_upstream("https://api.minimax.io/anthropic/v1", "/v1/messages", None).unwrap(),
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(
            join_upstream(
                "https://api.minimax.io/anthropic/v1",
                "/anthropic/v1/messages",
                None
            )
            .unwrap(),
            "https://api.minimax.io/anthropic/v1/messages"
        );
    }

    #[test]
    fn join_upstream_handles_root_and_query() {
        assert_eq!(
            join_upstream("https://api.minimax.io/v1", "/messages?x=1", None).unwrap(),
            "https://api.minimax.io/v1/messages?x=1"
        );
        assert_eq!(
            join_upstream("https://api.minimax.io/v1", "/", None).unwrap(),
            "https://api.minimax.io/v1"
        );
    }

    #[test]
    fn join_upstream_rejects_absolute_targets() {
        assert!(join_upstream("https://api.minimax.io", "https://other.example/x", None).is_err());
    }

    /// The proxy starts BEFORE the run command creates its run
    /// dir. Without a wait, the first LLM call lands as a 503.
    /// `resolve_log_path_blocking` polls for a run dir with backoff
    /// (50ms → 1s) up to 10s before giving up. This test seeds a
    /// run dir 250ms in and asserts that the function returns
    /// Some(_) once the dir appears. The polling interval
    /// sequence (50, 100, 200, 400, 800, ...) lands at 200ms and
    /// 400ms — the 250ms dir creation is caught by the 400ms tick.
    #[test]
    fn resolve_log_path_blocking_waits_for_run_dir_to_appear() {
        let tmp = tempfile::tempdir().unwrap();
        let runs_dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(runs_dir.join(".runs")).unwrap();
        let cfg = ProxyConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream: "https://api.minimax.io/anthropic/v1".into(),
            runs_dir: runs_dir.clone(),
            run_id: None,
            include_bodies: true,
            upstream_timeout: Duration::from_secs(180),
            max_body_bytes: 32 * 1024 * 1024,
            refuse_loopback_forward: false,
            refuse_loopback_forward_allowed: true,
            fixed_log_path: None,
        };
        let runs_dir_for_thread = runs_dir.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            std::fs::create_dir(
                runs_dir_for_thread
                    .join(".runs")
                    .join("01900000-0000-7000-8000-000000000001"),
            )
            .unwrap();
        });
        let result = resolve_log_path_blocking(&cfg).unwrap();
        handle.join().unwrap();
        let path = result.expect("proxy must wait for the run dir");
        assert!(path.ends_with("external_audit.jsonl.gz"));
    }

    /// `resolve_log_path` (the non-blocking form used by
    /// `start_proxy`'s startup banner) returns None immediately
    /// when no run exists. It must NOT block — otherwise the
    /// startup banner would time out before the run command
    /// can create its dir, and integration tests that wait for
    /// the banner line would fail.
    #[test]
    fn resolve_log_path_returns_none_immediately_when_no_run_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let runs_dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(runs_dir.join(".runs")).unwrap();
        let cfg = ProxyConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream: "https://api.minimax.io/anthropic/v1".into(),
            runs_dir,
            run_id: None,
            include_bodies: true,
            upstream_timeout: Duration::from_secs(180),
            max_body_bytes: 32 * 1024 * 1024,
            refuse_loopback_forward: false,
            refuse_loopback_forward_allowed: true,
            fixed_log_path: None,
        };
        let started = std::time::Instant::now();
        let result = resolve_log_path(&cfg).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "non-blocking variant must not sleep; took {:?}",
            started.elapsed()
        );
        assert!(result.is_none());
    }
}
