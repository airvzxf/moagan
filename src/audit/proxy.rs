//! HTTP/1.1 forwarder used by `moagan audit proxy`.
//!
//! The sidecar is a small, blocking-I/O TCP server built on
//! `std::net::TcpListener` and `std::io::Read`/`Write`. The AGENTS.md
//! no-go list forbids `axum` and `hyper`, so the proxy implements
//! only the subset of HTTP/1.1 we actually need:
//!
//! - request line + headers terminated by `\r\n\r\n`;
//! - body via `Content-Length` or `Transfer-Encoding: chunked`;
//! - response relayed verbatim to the client, with the body read
//!   into memory (we cap it at 32 MiB by default to avoid DoS).
//!
//! The forwarder is fully driven by `tokio::task::spawn_blocking`
//! because the I/O surface is `std::net`. It uses a `reqwest::Client`
//! (the same builder as the rest of the CLI, see
//! `src/llm/http.rs:16`) for the upstream leg.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reqwest::redirect::Policy;
use tokio::sync::Notify;

use crate::error::{Error, IoError, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;

use super::format::{AuditRecord, AuditWriter, body_canonical, redact_header, sha256_hex};

/// Configures the sidecar.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Address to bind on. `127.0.0.1:0` lets the kernel assign.
    pub listen: SocketAddr,
    /// Resolved upstream base URL (`https://api.minimax.io/anthropic/v1`).
    pub upstream: String,
    /// Root directory containing `<root>/.runs/`. The proxy derives
    /// the audit file path from this root together with `run_id`.
    /// Required.
    pub runs_dir: PathBuf,
    /// Pin the proxy to a specific run id. When `None`, the proxy
    /// watches `<runs_dir>/.runs/` for the most recently created
    /// run directory and starts writing to its audit file as soon as
    /// it appears.
    pub run_id: Option<RunId>,
    /// Whether to include `body_canonical` in the log. When `false`,
    /// only `body_sha256` and `body_size` are recorded.
    pub include_bodies: bool,
    /// Per-request timeout for the upstream call.
    pub upstream_timeout: Duration,
    /// Hard cap on the request body size in bytes.
    pub max_body_bytes: usize,
    /// Refuse to start if the upstream host matches the listen address.
    /// Prevents accidentally forwarding to ourselves in a loop.
    pub refuse_loopback_forward: bool,
    /// Allow loopback upstream when explicitly permitted by the CLI
    /// (used in tests and in the smoke harness). Default: false.
    pub refuse_loopback_forward_allowed: bool,
    /// Optional fixed log path. When set, the proxy always writes to
    /// this exact path regardless of `run_id` and `runs_dir`. Used by
    /// integration tests that want a stable file in `tempdir()`.
    pub fixed_log_path: Option<PathBuf>,
}

impl ProxyConfig {
    /// Build a config with sensible defaults for `max_body_bytes`
    /// (32 MiB) and `upstream_timeout` (180 s).
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

/// Resolved listen address + a handle used to stop the proxy. The
/// `JoinHandle` is exposed so callers can wait for the accept loop
/// to drain before exiting.
#[derive(Debug)]
pub struct ProxyHandle {
    /// Address the listener is bound to.
    pub local_addr: SocketAddr,
    /// Notifier that signals the accept loop to exit.
    shutdown: Arc<Notify>,
}

/// Pair of (writer, current run id). The accept loop reads the run
/// id to decide whether to swap the writer at the start of each
/// connection.
struct WriterSlot {
    writer: AuditWriter,
    current_run: Option<RunId>,
}

/// Start the sidecar. Returns once the listener is bound, after
/// printing the resolved address. The accept loop runs in a
/// background task; call [`ProxyHandle::shutdown`] to stop it.
///
/// When `cfg.run_id` is `None` and `cfg.fixed_log_path` is `None`,
/// the proxy starts a background poller that watches
/// `<cfg.runs_dir>/.runs/` for new run directories. Each new run
/// triggers an atomic swap of the audit writer, so all calls for a
/// single run land in that run's `<run>/telemetry/external_audit.jsonl`.
pub async fn start(cfg: ProxyConfig) -> Result<ProxyHandle> {
    if cfg.refuse_loopback_forward
        && !cfg.refuse_loopback_forward_allowed
        && let Some(host) = url_host(&cfg.upstream)
        && (host.eq_ignore_ascii_case("127.0.0.1") || host.eq_ignore_ascii_case("localhost"))
    {
        return Err(Error::InvalidArgs(format!(
            "refusing to forward to loopback upstream {}",
            cfg.upstream
        )));
    }
    let listener = TcpListener::bind(cfg.listen).map_err(|e| Error::Io(IoError::Raw(e)))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    let shutdown = Arc::new(Notify::new());
    let initial_path = resolve_initial_log_path(&cfg)?;
    let initial_run = initial_path_run_id(&initial_path);
    let writer = Arc::new(Mutex::new(WriterSlot {
        writer: AuditWriter::create(&initial_path).map_err(|e| Error::Io(IoError::Raw(e)))?,
        current_run: initial_run,
    }));
    let client = reqwest::Client::builder()
        .timeout(cfg.upstream_timeout)
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("moagan-audit/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::none())
        .build()
        .map_err(|e| Error::Provider(format!("build reqwest client: {e}")))?;
    let cfg = Arc::new(cfg);
    let shutdown_clone = Arc::clone(&shutdown);
    let writer_for_accept = Arc::clone(&writer);
    let cfg_for_accept = Arc::clone(&cfg);
    std::thread::spawn(move || {
        run_accept(
            listener,
            cfg_for_accept,
            writer_for_accept,
            client,
            shutdown_clone,
        );
    });
    if cfg.fixed_log_path.is_none() && cfg.run_id.is_none() {
        let poller_cfg = Arc::clone(&cfg);
        let poller_writer = Arc::clone(&writer);
        let poller_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            run_runs_dir_poller(poller_cfg, poller_writer, poller_shutdown);
        });
    }
    Ok(ProxyHandle {
        local_addr,
        shutdown,
    })
}

impl ProxyHandle {
    /// Signal the accept loop to stop and wait for in-flight
    /// handlers to finalise their writes.
    pub async fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }
}

/// Resolve the log path to use at startup.
///
/// Priority:
/// 1. `cfg.fixed_log_path` if set.
/// 2. `cfg.run_id`'s audit path if `cfg.run_id` is Some.
/// 3. The latest existing run under `<cfg.runs_dir>/.runs/`.
/// 4. A sentinel path under the runs dir that lets the very first
///    request get logged somewhere; the poller will swap to the
///    correct run as soon as it appears.
fn resolve_initial_log_path(cfg: &ProxyConfig) -> Result<PathBuf> {
    if let Some(p) = &cfg.fixed_log_path {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(IoError::Raw(e)))?;
        }
        return Ok(p.clone());
    }
    let runs_root = cfg.runs_dir.join(".runs");
    std::fs::create_dir_all(&runs_root).map_err(|e| Error::Io(IoError::Raw(e)))?;
    if let Some(run_id) = cfg.run_id {
        let home = MoaganHome::at(cfg.runs_dir.clone());
        let run_dir = home.run_dir(run_id);
        run_dir.ensure()?;
        return Ok(run_dir.external_audit_path());
    }
    if let Some(latest) = pick_latest_run(&runs_root)? {
        let home = MoaganHome::at(cfg.runs_dir.clone());
        let run_dir = home.run_dir(latest);
        run_dir.ensure()?;
        return Ok(run_dir.external_audit_path());
    }
    let sentinel = runs_root.join(".audit_pending");
    std::fs::create_dir_all(&sentinel).map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(sentinel.join("external_audit.jsonl"))
}

/// Extract the run id from a log path. Returns None for sentinel paths.
fn initial_path_run_id(path: &Path) -> Option<RunId> {
    // Path looks like <runs_dir>/.runs/<uuid>/telemetry/external_audit.jsonl
    let components: Vec<_> = path.components().collect();
    if components.len() < 3 {
        return None;
    }
    // Walk back from the end until we find a parseable UUID.
    for c in components.iter().rev() {
        if let std::path::Component::Normal(s) = c
            && let Ok(id) = s.to_string_lossy().parse::<RunId>()
        {
            return Some(id);
        }
    }
    None
}

/// Pick the highest-UUIDv7 run directory currently on disk.
fn pick_latest_run(runs_root: &Path) -> Result<Option<RunId>> {
    let mut entries: Vec<(RunId, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(runs_root).map_err(|e| Error::Io(IoError::Raw(e)))? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let id = match name.parse::<RunId>() {
            Ok(id) => id,
            Err(_) => continue,
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        entries.push((id, modified));
    }
    Ok(entries
        .into_iter()
        .max_by_key(|(id, _)| *id)
        .map(|(id, _)| id))
}

/// Background poller that watches `<runs_dir>/.runs/` for new run
/// directories. When a new run is detected, it closes the current
/// `AuditWriter` (flushing any buffered bytes) and opens a new one
/// for that run's audit file. The proxy then writes every
/// subsequent request to the new file.
///
/// Polling interval: 200 ms. This is the right granularity for
/// moagan's typical workload (a deep run creates the run directory
/// tens of milliseconds before the first HTTP call).
fn run_runs_dir_poller(
    cfg: Arc<ProxyConfig>,
    writer: Arc<Mutex<WriterSlot>>,
    shutdown: Arc<Notify>,
) {
    let runs_root = cfg.runs_dir.join(".runs");
    eprintln!(
        "audit poller: started, polling every 50 ms in {}",
        runs_root.display()
    );
    let interval = Duration::from_millis(50);
    loop {
        std::thread::sleep(interval);
        if shutdown_has_fired(&shutdown) {
            break;
        }
        sync_writer_to_latest_run(&cfg, &writer);
    }
}

fn run_accept(
    listener: TcpListener,
    cfg: Arc<ProxyConfig>,
    writer: Arc<Mutex<WriterSlot>>,
    client: reqwest::Client,
    shutdown: Arc<Notify>,
) {
    listener
        .set_nonblocking(false)
        .expect("reset blocking listener");
    let dynamic = cfg.fixed_log_path.is_none() && cfg.run_id.is_none();
    loop {
        let (stream, _peer) = match listener.accept() {
            Ok(pair) => pair,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("warn: audit proxy accept error: {e}");
                continue;
            }
        };
        if shutdown_has_fired(&shutdown) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            break;
        }
        // In dynamic-discovery mode, swap the writer to the latest
        // run *synchronously* before the connection handler reads
        // its first byte. The background poller (50 ms cadence)
        // handles steady-state, but at startup the proxy can miss
        // the first call by the full poll interval.
        if dynamic {
            sync_writer_to_latest_run(&cfg, &writer);
        }
        let cfg = Arc::clone(&cfg);
        let writer = Arc::clone(&writer);
        let client = client.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build audit tokio runtime");
            let _ = rt.block_on(handle_connection(stream, cfg, writer, client));
        });
    }
}

/// Swap the shared writer to the latest run directory if a newer one
/// has appeared since the last swap. Idempotent and cheap when the
/// run id has not changed (a single hashmap lookup).
fn sync_writer_to_latest_run(cfg: &ProxyConfig, writer: &Mutex<WriterSlot>) {
    let runs_root = cfg.runs_dir.join(".runs");
    let latest = match pick_latest_run(&runs_root) {
        Ok(Some(id)) => id,
        _ => return,
    };
    {
        let slot = writer.lock();
        if slot.current_run == Some(latest) {
            return;
        }
    }
    let target = MoaganHome::at(cfg.runs_dir.clone())
        .run_dir(latest)
        .external_audit_path();
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match AuditWriter::append(&target) {
        Ok(new_writer) => {
            let mut slot = writer.lock();
            // Re-check under the lock to avoid two threads racing
            // to install different writers.
            if slot.current_run != Some(latest) {
                *slot = WriterSlot {
                    writer: new_writer,
                    current_run: Some(latest),
                };
                eprintln!("audit proxy: writer swapped to run {latest}");
            }
        }
        Err(e) => {
            eprintln!("warn: audit proxy writer swap failed: {e}");
        }
    }
}

fn shutdown_has_fired(notify: &Notify) -> bool {
    let notified = notify.notified();
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match std::pin::Pin::new(&mut Box::pin(notified)).poll(&mut cx) {
        std::task::Poll::Ready(()) => true,
        std::task::Poll::Pending => false,
    }
}

async fn handle_connection(
    stream: TcpStream,
    cfg: Arc<ProxyConfig>,
    writer: Arc<Mutex<WriterSlot>>,
    client: reqwest::Client,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(60))).ok();
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| Error::Io(IoError::Raw(e)))?);
    let mut writer_stream = stream;

    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    if request_line.is_empty() {
        return Ok(());
    }
    let mut parts = request_line.trim_end_matches(['\r', '\n']).split(' ');
    let method = parts.next().unwrap_or("GET").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    let http_version = parts.next().unwrap_or("HTTP/1.1").to_owned();

    let mut headers = BTreeMap::new();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| Error::Io(IoError::Raw(e)))?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some((k, v)) = trimmed.split_once(':') {
            let name = k.trim().to_ascii_lowercase();
            let value = v.trim().to_owned();
            if name == "content-length" {
                content_length = value.parse().ok();
            } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
            headers.insert(name, value);
        }
    }

    let body_bytes = if chunked {
        read_chunked_body(&mut reader, cfg.max_body_bytes)?
    } else if let Some(n) = content_length {
        read_n_body(&mut reader, n, cfg.max_body_bytes)?
    } else {
        Vec::new()
    };
    if body_bytes.len() > cfg.max_body_bytes {
        return write_error(&mut writer_stream, 413, "payload too large");
    }

    let id = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let started = Instant::now();
    let upstream_url = join_upstream(
        &cfg.upstream,
        &path,
        headers.get("host").map(String::as_str),
    )?;
    let body_sha = sha256_hex(&body_bytes);
    let body_canon = if cfg.include_bodies {
        Some(body_canonical(&body_bytes))
    } else {
        None
    };
    let redacted_req_headers: BTreeMap<String, String> = headers
        .iter()
        .map(|(k, v)| (k.clone(), redact_header(k, v)))
        .collect();
    let req_rec = AuditRecord {
        ts: unix_now(),
        event: "request".into(),
        id: id.clone(),
        method: Some(method.clone()),
        url: Some(upstream_url.clone()),
        status: None,
        headers: redacted_req_headers,
        body_canonical: body_canon.clone(),
        body_sha256: body_sha.clone(),
        body_size: body_bytes.len() as u64,
        elapsed_ms: None,
        crc32: String::new(),
        error: None,
    };
    {
        let mut slot = writer.lock();
        let mut r = req_rec.clone();
        slot.writer.write_record(&mut r).map_err(Error::from)?;
        slot.writer.flush_gz().map_err(Error::from)?;
    }

    let mut fwd_headers = reqwest::header::HeaderMap::new();
    for (k, v) in &headers {
        if matches!(
            k.as_str(),
            "host" | "content-length" | "connection" | "accept-encoding" | "transfer-encoding"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            fwd_headers.insert(name, value);
        }
    }

    let method_typed = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| Error::Provider(format!("method: {e}")))?;
    let request_builder = client
        .request(method_typed, &upstream_url)
        .headers(fwd_headers)
        .body(body_bytes.clone());
    let response_result = request_builder.send().await;
    let response = match response_result {
        Ok(r) => r,
        Err(e) => {
            let err_rec = AuditRecord {
                ts: unix_now(),
                event: "upstream_error".into(),
                id: id.clone(),
                method: None,
                url: None,
                status: None,
                headers: BTreeMap::new(),
                body_canonical: None,
                body_sha256: String::new(),
                body_size: 0,
                elapsed_ms: None,
                crc32: String::new(),
                error: Some(e.to_string()),
            };
            {
                let mut slot = writer.lock();
                let mut r = err_rec;
                slot.writer.write_record(&mut r).map_err(Error::from)?;
                slot.writer.flush_gz().map_err(Error::from)?;
            }
            return write_error(&mut writer_stream, 502, "upstream error");
        }
    };

    let status = response.status().as_u16();
    let mut resp_headers = BTreeMap::new();
    for (k, v) in response.headers().iter() {
        let value = v.to_str().unwrap_or("<binary>").to_owned();
        resp_headers.insert(
            k.as_str().to_ascii_lowercase(),
            redact_header(k.as_str(), &value),
        );
    }
    let mut resp_body: Vec<u8> = Vec::new();
    let read_result = read_response_body(&mut resp_body, response, cfg.max_body_bytes).await;
    if let Err(e) = read_result {
        let err_rec = AuditRecord {
            ts: unix_now(),
            event: "upstream_error".into(),
            id: id.clone(),
            method: None,
            url: None,
            status: None,
            headers: BTreeMap::new(),
            body_canonical: None,
            body_sha256: String::new(),
            body_size: 0,
            elapsed_ms: None,
            crc32: String::new(),
            error: Some(e.to_string()),
        };
        {
            let mut slot = writer.lock();
            let mut r = err_rec;
            slot.writer.write_record(&mut r).map_err(Error::from)?;
        }
        return write_error(&mut writer_stream, 502, "upstream body read error");
    }
    let resp_body_sha = sha256_hex(&resp_body);
    let resp_body_canon = if cfg.include_bodies {
        Some(body_canonical(&resp_body))
    } else {
        None
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let resp_rec = AuditRecord {
        ts: unix_now(),
        event: "response".into(),
        id: id.clone(),
        method: None,
        url: None,
        status: Some(status),
        headers: resp_headers,
        body_canonical: resp_body_canon,
        body_sha256: resp_body_sha,
        body_size: resp_body.len() as u64,
        elapsed_ms: Some(elapsed_ms),
        crc32: String::new(),
        error: None,
    };
    {
        let mut slot = writer.lock();
        let mut r = resp_rec;
        slot.writer.write_record(&mut r).map_err(Error::from)?;
        slot.writer.flush_gz().map_err(Error::from)?;
    }

    let resp_line = format!("{http_version} {status} {}\r\n", reason_phrase(status));
    writer_stream
        .write_all(resp_line.as_bytes())
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    writer_stream
        .write_all(format!("Content-Length: {}\r\n", resp_body.len()).as_bytes())
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    writer_stream
        .write_all(b"Connection: close\r\n\r\n")
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    writer_stream
        .write_all(&resp_body)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    writer_stream
        .flush()
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(())
}

async fn read_response_body(
    buf: &mut Vec<u8>,
    response: reqwest::Response,
    cap: usize,
) -> std::result::Result<(), reqwest::Error> {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buf.len() + chunk.len() > cap {
            let remaining = cap - buf.len();
            buf.extend_from_slice(&chunk[..remaining]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(())
}

fn read_n_body<R: Read>(reader: &mut R, n: usize, cap: usize) -> Result<Vec<u8>> {
    if n > cap {
        return Err(Error::Provider(format!(
            "content-length {n} exceeds cap {cap}"
        )));
    }
    let mut buf = vec![0u8; n];
    reader
        .read_exact(&mut buf)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(buf)
}

fn read_chunked_body<R: BufRead>(reader: &mut R, cap: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| Error::Io(IoError::Raw(e)))?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let size_str = trimmed.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|e| Error::Provider(format!("chunk size: {e}")))?;
        if size == 0 {
            let mut tail = String::new();
            reader
                .read_line(&mut tail)
                .map_err(|e| Error::Io(IoError::Raw(e)))?;
            break;
        }
        if out.len() + size > cap {
            return Err(Error::Provider("chunked body exceeds cap".into()));
        }
        let mut buf = vec![0u8; size];
        reader
            .read_exact(&mut buf)
            .map_err(|e| Error::Io(IoError::Raw(e)))?;
        out.extend_from_slice(&buf);
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|e| Error::Io(IoError::Raw(e)))?;
    }
    Ok(out)
}

fn write_error<W: Write>(stream: &mut W, status: u16, msg: &str) -> Result<()> {
    let body = format!("{status} {msg}\n");
    let line = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    stream
        .write_all(line.as_bytes())
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    stream
        .write_all(format!("Content-Length: {}\r\n", body.len()).as_bytes())
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    stream
        .write_all(b"Connection: close\r\n\r\n")
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    stream
        .write_all(body.as_bytes())
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    stream.flush().map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "OK",
    }
}

fn join_upstream(base: &str, path: &str, _host_hint: Option<&str>) -> Result<String> {
    if path.starts_with("http://") || path.starts_with("https://") {
        return Ok(path.to_owned());
    }
    let trimmed = base.trim_end_matches('/');
    if path == "/" {
        return Ok(trimmed.to_owned());
    }
    if let Some(stripped) = trimmed.strip_suffix("/anthropic/v1") {
        if path.starts_with("/anthropic/v1/") {
            // After the prefix `/anthropic/v1` (12 chars + trailing
            // slash), the remainder starts at index 13.
            return Ok(format!("{stripped}{}", &path[13..]));
        }
        if path.starts_with("/v1/") {
            return Ok(format!("{stripped}{}", &path[3..]));
        }
    }
    if trimmed.ends_with("/v1") && path.starts_with("/v1/") {
        return Ok(format!("{trimmed}{}", &path[3..]));
    }
    Ok(format!("{trimmed}{path}"))
}

fn url_host(url: &str) -> Option<String> {
    let after = url.split("://").nth(1)?;
    let host = after.split('/').next()?.split(':').next()?;
    Some(host.to_owned())
}

/// Wall-clock seconds since the Unix epoch. Rounded to microsecond
/// precision so the value round-trips through `serde_json` without
/// losing bits: `SystemTime::as_secs_f64` has nanosecond precision but
/// `f64` cannot represent all of those decimals exactly, so a naive
/// value would serialise to e.g. `1785199064.2119439` but parse back
/// as a slightly different f64 that re-serialises to
/// `1785199064.211944`. The mismatch breaks the per-line CRC. The
/// microsecond round keeps the value well within `serde_json`'s
/// shortest-decimal guarantee (6 decimals).
fn unix_now() -> f64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let micros = (d.as_secs_f64() * 1_000_000.0).round();
    micros / 1_000_000.0
}

#[allow(dead_code)]
fn _resolve_loopback(a: &str) -> Option<SocketAddr> {
    a.to_socket_addrs().ok().and_then(|mut i| i.next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_phrase_known() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(502), "Bad Gateway");
    }

    #[test]
    fn join_upstream_handles_root() {
        assert_eq!(
            join_upstream("https://api.minimax.io", "/", None).unwrap(),
            "https://api.minimax.io"
        );
        assert_eq!(
            join_upstream("https://api.minimax.io/", "/messages", None).unwrap(),
            "https://api.minimax.io/messages"
        );
    }

    #[test]
    fn join_upstream_passthrough_absolute() {
        assert_eq!(
            join_upstream("https://api.minimax.io", "https://other.example/x", None).unwrap(),
            "https://other.example/x"
        );
    }

    #[test]
    fn url_host_parses_https() {
        assert_eq!(
            url_host("https://api.minimax.io/x"),
            Some("api.minimax.io".into())
        );
        assert_eq!(
            url_host("https://api.minimax.io:8080/x"),
            Some("api.minimax.io".into())
        );
    }
}
