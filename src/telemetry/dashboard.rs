//! `moagan telemetry view` — read-only HTTP dashboard bound on
//! 127.0.0.1.
//!
//! Implements the seven endpoints from `proposal-02-rust.md §10.8`
//! + `V4 §8.8`:
//!
//! - `GET /api/runs`
//! - `GET /api/lineage` (J#5 closure — cross-run parent/child DAG).
//! - `GET /api/runs/<run_id>`
//! - `GET /api/runs/<run_id>/phases`
//! - `GET /api/runs/<run_id>/calls`
//! - `GET /api/runs/<run_id>/provider_usage`
//! - `GET /api/runs/<run_id>/hashes`
//! - `GET /api/runs/<run_id>/export?level=summary&format=tar.gz`
//!
//! The server is built directly on `tokio::net::TcpListener` with
//! a hand-rolled HTTP/1.1 parser (no `axum` / `hyper` per the
//! no-go list). The same pattern is already used for
//! `moagan audit proxy` (commit #6); the dashboard is GET-only
//! and read-only, so the surface is smaller.
//!
//! Connection policy:
//! - bound on 127.0.0.1 only (loopback).
//! - default port 4096; the caller can override and the server
//!   searches up to N ports in the blacklist to find a free one.
//! - blacklist `[22, 80, 443, 3306, 5432, 6379, 8080, 8443]` per
//!   V4 §8.8.
//! - request size hard-capped at 8 KiB (GET requests should be
//!   one-liners).
//! - per-request IO timeout 30s.
//! - graceful shutdown via `CancellationToken`.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::cli::telemetry_cmd::{ExportFormat, ExportLevel};
use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::storage::sqlite::Db;
use crate::telemetry::export::{self, ExportResult};
use crate::telemetry::lineage_graph::LineageGraph;

/// Maximum number of lines / bytes for a single HTTP request.
/// GET requests should fit in a single URL + a couple of
/// headers; 8 KiB is generous.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Default port (V4 §8.8).
pub const DEFAULT_PORT: u16 = 4096;

/// Ports the server will skip when searching for a free slot.
pub const PORT_BLACKLIST: &[u16] = &[22, 80, 443, 3306, 5432, 6379, 8080, 8443];

/// Per-request IO timeout.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Bind the dashboard and run it until `shutdown` fires.
pub struct DashboardHandle {
    /// Address actually bound (port may differ from the request
    /// when an override picked a fallback).
    pub local_addr: SocketAddr,
    shutdown: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
}

impl std::fmt::Debug for DashboardHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DashboardHandle")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl DashboardHandle {
    /// Stop accepting connections and drain active handlers.
    pub async fn shutdown(mut self) -> Result<()> {
        tracing::info!(local_addr = %self.local_addr, "DashboardHandle::shutdown: enter");
        self.shutdown.cancel();
        let Some(task) = self.task.take() else {
            tracing::info!("DashboardHandle::shutdown: no task to drain");
            return Ok(());
        };
        let res = task.await;
        match res {
            Ok(Ok(())) => {
                tracing::info!("DashboardHandle::shutdown: task finished cleanly");
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "DashboardHandle::shutdown: task errored");
                Err(e)
            }
            Err(e) => {
                tracing::error!(error = %e, "DashboardHandle::shutdown: join error");
                Err(Error::InvalidState(format!("dashboard task failed: {e}")))
            }
        }
    }
}

impl Drop for DashboardHandle {
    fn drop(&mut self) {
        tracing::debug!("DashboardHandle::drop: cancelling shutdown token");
        self.shutdown.cancel();
    }
}

/// Configuration for the dashboard server.
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    /// Bind host. Hard-coded to `127.0.0.1` per V4 §8.8; the field
    /// is kept for symmetry with the audit proxy and to allow
    /// tests to point at `::1` if needed.
    pub bind: SocketAddr,
    /// Moagan home (drives both the SQLite index and the
    /// per-run directories the export endpoint reads).
    pub home: Arc<MoaganHome>,
    /// Optional explicit path for `meta.sqlite`. When `None` the
    /// server opens `home.meta_db_path()` lazily.
    pub db_path: Option<PathBuf>,
}

impl DashboardConfig {}

/// Resolve the port-search behaviour: try `bind.port` first;
/// if it is taken, walk forward through the free ports and
/// skip the blacklist.
fn pick_port(requested: u16) -> u16 {
    if requested == 0 {
        return 0;
    }
    if !PORT_BLACKLIST.contains(&requested) {
        return requested;
    }
    for offset in 1..=1000 {
        let candidate = requested.saturating_add(offset);
        if !PORT_BLACKLIST.contains(&candidate) {
            return candidate;
        }
    }
    requested
}

/// Start the dashboard. `bind` must be a loopback address.
pub async fn start(cfg: DashboardConfig) -> Result<DashboardHandle> {
    let bind_ip = cfg.bind.ip();
    tracing::info!(bind = %cfg.bind, "Dashboard::start: enter");
    if !bind_ip.is_loopback() {
        tracing::error!(bind = %cfg.bind, "Dashboard::start: non-loopback bind rejected");
        return Err(Error::InvalidArgs(
            "dashboard must bind on a loopback address".into(),
        ));
    }
    let bind = SocketAddr::new(bind_ip, pick_port(cfg.bind.port()));
    let listener = TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    let cfg = Arc::new(cfg);
    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(serve(listener, cfg, task_shutdown));
    tracing::info!(
        local_addr = %local_addr,
        "Dashboard::start: serving on local addr"
    );
    Ok(DashboardHandle {
        local_addr,
        shutdown,
        task: Some(task),
    })
}

async fn serve(
    listener: TcpListener,
    cfg: Arc<DashboardConfig>,
    shutdown: CancellationToken,
) -> Result<()> {
    tracing::debug!("dashboard::serve: accept loop starting");
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("dashboard::serve: shutdown signalled");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!(error = %e, "dashboard::serve: accept failed");
                        continue;
                    }
                };
                let cfg = Arc::clone(&cfg);
                let handler_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    if let Err(e) = tokio::time::timeout(
                        IO_TIMEOUT,
                        handle_connection(stream, cfg, handler_shutdown),
                    )
                    .await
                    {
                        tracing::debug!(error = %e, "dashboard handler timed out or errored");
                    }
                });
            }
        }
    }
}

#[derive(Debug)]
struct ParsedRequest {
    method: String,
    target: String,
    version: String,
}

async fn handle_connection(
    mut stream: TcpStream,
    cfg: Arc<DashboardConfig>,
    shutdown: CancellationToken,
) -> Result<()> {
    if shutdown.is_cancelled() {
        tracing::trace!("dashboard::handle_connection: cancelled");
        return Ok(());
    }
    let request = match read_request(&mut stream).await {
        Ok(r) => r,
        Err(status) => {
            tracing::debug!(status, "dashboard::handle_connection: bad request");
            return write_error(&mut stream, status, "bad request").await;
        }
    };
    if request.method != "GET" {
        tracing::debug!(method = %request.method, "dashboard::handle_connection: non-GET rejected");
        return write_error(&mut stream, 405, "method not allowed").await;
    }
    let (path, query) = request
        .target
        .split_once('?')
        .map_or((request.target.as_str(), ""), |(p, q)| (p, q));
    tracing::debug!(path, query, "dashboard::handle_connection: dispatch");
    let response = dispatch(path, query, &cfg).await;
    match response {
        Ok(resp) => write_response(&mut stream, &request.version, &resp).await,
        Err((status, msg)) => {
            tracing::debug!(path, status, message = %msg, "dashboard::handle_connection: dispatch error");
            write_error(&mut stream, status, &msg).await
        }
    }
}

/// One HTTP response body. Body is bytes (UTF-8 JSON in practice).
#[derive(Debug)]
struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

async fn dispatch(
    path: &str,
    query: &str,
    cfg: &DashboardConfig,
) -> std::result::Result<Response, (u16, String)> {
    tracing::trace!(path, "dashboard::dispatch: enter");
    let db = open_db(cfg).map_err(internal)?;

    if path == "/api/runs" {
        let limit = parse_query_u64(query, "limit").unwrap_or(50).min(1000);
        let rows = db.list_runs(limit as u32).map_err(internal)?;
        return Ok(json_response(
            200,
            &serde_json::to_vec(&rows).map_err(internal)?,
        ));
    }
    // Cross-run lineage view (J#5 closure). Walks the `runs` table
    // for `parent_run_id` <-> `run_id` pairs and projects them onto a
    // [`LineageGraph`] adjacency list. Parents without a recorded
    // `parent_run_id` become childless roots and still show up as
    // nodes (no edges attached).
    if path == "/api/lineage" {
        let rows = db.list_lineage_pairs().map_err(internal)?;
        let pairs: Vec<(String, String)> = rows
            .iter()
            .filter_map(|(parent, child)| {
                let parent = parent.as_ref()?;
                let child = child.as_ref()?;
                Some((parent.clone(), child.clone()))
            })
            .collect();
        let graph = LineageGraph::from_pairs(&pairs);
        return Ok(json_response(
            200,
            &serde_json::to_vec(&graph).map_err(internal)?,
        ));
    }
    // Cross-run diff view (D.17 dashboard extension). Accepts
    // `?ids=uuid1,uuid2[,uuid3,...]` and returns per-run slices
    // plus the cross-run diff (max/min duration, max error
    // count, per-provider token totals, per-provider error
    // rates). Reuses [`Db::compare_runs`]; the 400/404 paths are
    // pinned here so the SQLite helper stays purely
    // aggregation-focused.
    if path == "/api/compare-runs" {
        let ids = parse_query_csv(query, "ids");
        if ids.len() < 2 {
            return Err((
                400,
                format!("compare-runs requires at least 2 ids (got {})", ids.len()),
            ));
        }
        let mut run_ids: Vec<RunId> = Vec::with_capacity(ids.len());
        for raw in &ids {
            let parsed: RunId = raw
                .parse()
                .map_err(|e| (400, format!("invalid run id '{raw}': {e}")))?;
            run_ids.push(parsed);
        }
        // 404 path: the SQLite helper silently skips missing
        // runs from the per-provider breakdown, so we have to
        // validate existence here before delegating.
        for run_id in &run_ids {
            if !db.has_run(*run_id).map_err(internal)? {
                return Err((404, format!("run {run_id} not found")));
            }
        }
        let response = db.compare_runs(&run_ids).map_err(internal)?;
        return Ok(json_response(
            200,
            &serde_json::to_vec(&response).map_err(internal)?,
        ));
    }
    // Cross-run rolling-window aggregate. Accepts an optional
    // `since=YYYY-MM-DD` (calendar date, midnight UTC) and an
    // optional `provider=NAME` filter. `since` defaults to
    // seven days before the request; `provider` defaults to
    // `None` (every provider in the window).
    if path == "/api/aggregates" {
        let now = crate::time::now_unix_secs();
        let since_unix =
            parse_query_since(query, "since").unwrap_or_else(|| now.saturating_sub(7 * 86_400));
        let provider = parse_query_string(query, "provider");
        let aggregate = db
            .aggregates_window(since_unix, provider.as_deref())
            .map_err(internal)?;
        return Ok(json_response(
            200,
            &serde_json::to_vec(&aggregate).map_err(internal)?,
        ));
    }
    if let Some(rest) = path.strip_prefix("/api/runs/") {
        // /api/runs/<id>[/suffix]
        let (run_id_str, suffix) = match rest.split_once('/') {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };
        let run_id: RunId = run_id_str
            .parse()
            .map_err(|e| (400, format!("invalid run id '{run_id_str}': {e}")))?;
        match suffix {
            None => {
                let row = db
                    .get_run(run_id)
                    .map_err(internal)?
                    .ok_or_else(|| (404, format!("run {run_id} not found")))?;
                let agg = db.run_aggregate(run_id).map_err(internal)?;
                let phases = db.list_phase_summaries_for_run(run_id).map_err(internal)?;
                let usage = db.list_provider_usage_for_run(run_id).map_err(internal)?;
                let body = serde_json::json!({
                    "run": row,
                    "aggregate": agg,
                    "phases": phases,
                    "provider_usage": usage,
                });
                return Ok(json_response(
                    200,
                    &serde_json::to_vec(&body).map_err(internal)?,
                ));
            }
            Some("phases") => {
                let rows = db.list_phase_summaries_for_run(run_id).map_err(internal)?;
                return Ok(json_response(
                    200,
                    &serde_json::to_vec(&rows).map_err(internal)?,
                ));
            }
            Some("calls") => {
                let rows = db.list_calls_for_run(run_id).map_err(internal)?;
                return Ok(json_response(
                    200,
                    &serde_json::to_vec(&rows).map_err(internal)?,
                ));
            }
            Some("provider_usage") => {
                let rows = db.list_provider_usage_for_run(run_id).map_err(internal)?;
                return Ok(json_response(
                    200,
                    &serde_json::to_vec(&rows).map_err(internal)?,
                ));
            }
            Some("hashes") => {
                let run_dir = cfg.home.run_dir(run_id);
                if !run_dir.root().exists() {
                    return Err((
                        404,
                        format!("run dir not found at {}", run_dir.root().display()),
                    ));
                }
                let rows = compute_hashes(run_dir.root()).map_err(internal)?;
                return Ok(json_response(
                    200,
                    &serde_json::to_vec(&rows).map_err(internal)?,
                ));
            }
            Some("export") => {
                let level = parse_query_export_level(query).map_err(|e| (400, format!("{e}")))?;
                let format = parse_query_export_format(query).map_err(|e| (400, format!("{e}")))?;
                let run_dir = cfg.home.run_dir(run_id);
                if !run_dir.root().exists() {
                    return Err((
                        404,
                        format!("run dir not found at {}", run_dir.root().display()),
                    ));
                }
                let tmp = tempfile::tempdir().map_err(internal)?;
                let dest = tmp
                    .path()
                    .join(format!("run_{}_{}.{}", run_id.short(), level, format));
                let result: ExportResult =
                    export::export_run(&run_dir, run_id, level, format, &dest).map_err(internal)?;
                // Stream the binary archive instead of a JSON
                // summary. The pre-fix code returned the
                // `result.archive_path` and a `file_count` etc.,
                // which made the endpoint useless for a browser
                // (no way to download the file). The `/export-info`
                // endpoint below preserves the old shape for
                // backwards-compat consumers.
                let body = std::fs::read(&result.archive_path).map_err(internal)?;
                let content_type = match format {
                    ExportFormat::TarGz => "application/gzip",
                    ExportFormat::Tar => "application/x-tar",
                    ExportFormat::Zip => "application/zip",
                    ExportFormat::TarZst => "application/zstd",
                };
                return Ok(binary_response(200, content_type, body));
            }
            Some("export-info") => {
                // Backwards-compat endpoint: returns the JSON
                // summary that the `/export` endpoint used to
                // emit. Useful for callers that want to know
                // `file_count` / `archive_sha256` without
                // downloading the whole payload. The new
                // `/export` returns the binary itself.
                let level = parse_query_export_level(query).map_err(|e| (400, format!("{e}")))?;
                let format = parse_query_export_format(query).map_err(|e| (400, format!("{e}")))?;
                let run_dir = cfg.home.run_dir(run_id);
                if !run_dir.root().exists() {
                    return Err((
                        404,
                        format!("run dir not found at {}", run_dir.root().display()),
                    ));
                }
                let tmp = tempfile::tempdir().map_err(internal)?;
                let dest = tmp
                    .path()
                    .join(format!("run_{}_{}.{}", run_id.short(), level, format));
                let result: ExportResult =
                    export::export_run(&run_dir, run_id, level, format, &dest).map_err(internal)?;
                let body = serde_json::json!({
                    "archive_path": result.archive_path,
                    "file_count": result.file_count,
                    "archive_sha256": result.archive_sha256,
                    "payload_bytes": result.payload_bytes,
                    "archive_bytes": result.archive_bytes,
                });
                return Ok(json_response(
                    200,
                    &serde_json::to_vec(&body).map_err(internal)?,
                ));
            }
            Some(other) => return Err((404, format!("unknown sub-resource '{other}'"))),
        }
    }
    if path == "/" {
        return Ok(plain_text(200, DASHBOARD_INDEX));
    }
    Err((404, format!("not found: {path}")))
}

fn open_db(cfg: &DashboardConfig) -> Result<Db> {
    let path = cfg
        .db_path
        .clone()
        .unwrap_or_else(|| cfg.home.meta_db_path());
    tracing::trace!(path = %path.display(), "dashboard::open_db: enter");
    Db::open(&path)
}

fn internal<E: std::fmt::Display>(e: E) -> (u16, String) {
    (500, format!("internal: {e}"))
}

fn json_response(status: u16, body: &[u8]) -> Response {
    Response {
        status,
        content_type: "application/json; charset=utf-8",
        body: body.to_vec(),
    }
}

fn plain_text(status: u16, body: &str) -> Response {
    Response {
        status,
        content_type: "text/plain; charset=utf-8",
        body: body.as_bytes().to_vec(),
    }
}

/// Build a binary `Response` (no charset). Used by the export
/// endpoint to stream an `application/gzip` / `application/x-tar`
/// / `application/zip` payload to the client instead of a JSON
/// summary.
fn binary_response(status: u16, content_type: &'static str, body: Vec<u8>) -> Response {
    Response {
        status,
        content_type,
        body,
    }
}

/// Minimal HTML index page that points at the JSON endpoints.
/// Keeps the dashboard useful even without a separate SPA.
const DASHBOARD_INDEX: &str = "\
moagan dashboard

JSON endpoints:
  GET /api/runs
  GET /api/lineage
  GET /api/compare-runs?ids=<uuid1>,<uuid2>[,<uuid3>,...]
  GET /api/aggregates?since=YYYY-MM-DD&provider=<name>
  GET /api/runs/<id>
  GET /api/runs/<id>/phases
  GET /api/runs/<id>/calls
  GET /api/runs/<id>/provider_usage
  GET /api/runs/<id>/hashes
  GET /api/runs/<id>/export?level=summary|full&format=tar.gz|tar|zip
  GET /api/runs/<id>/export-info?level=summary|full&format=tar.gz|tar|zip
";

/// One row in the per-run hashes endpoint. Pure read-only; the
/// `sha256_file` helper in `export.rs` does the heavy lifting
/// but we project the result into this slim struct for the
/// dashboard.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct HashRow {
    /// Path relative to the run directory.
    pub path: String,
    /// SHA-256 hex.
    pub sha256: String,
    /// File size in bytes.
    pub bytes: u64,
}

/// Walk `root` recursively and compute the SHA-256 of every file.
/// Symlinks are skipped (matches the audit-sidecar policy).
pub fn compute_hashes(root: &std::path::Path) -> Result<Vec<HashRow>> {
    tracing::debug!(root = %root.display(), "compute_hashes: enter");
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if rel.is_empty() {
            continue;
        }
        let sha = export::sha256_file(path)?;
        out.push(HashRow {
            path: rel,
            sha256: sha,
            bytes: meta.len(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    tracing::debug!(count = out.len(), "compute_hashes: ok");
    Ok(out)
}

fn parse_query_u64(query: &str, key: &str) -> Option<u64> {
    for part in query.split('&') {
        let (k, v) = part.split_once('=')?;
        if k == key {
            return v.parse().ok();
        }
    }
    None
}

/// Pull a comma-separated string list from the query (e.g.
/// `ids=a,b,c`). Empty / absent fields collapse to an empty
/// `Vec`; empty segments (`ids=a,,b`) are skipped silently so a
/// trailing comma doesn't 400 a benign request. Used by
/// `/api/compare-runs` to parse the `ids` parameter.
fn parse_query_csv(query: &str, key: &str) -> Vec<String> {
    let raw = parse_query_string(query, key).unwrap_or_default();
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Pull a single string field from the query. Returns `None`
/// when the field is absent. Used by `/api/aggregates` for the
/// `provider` filter and by `/api/compare-runs` for the `ids`
/// raw value (before CSV splitting).
fn parse_query_string(query: &str, key: &str) -> Option<String> {
    for part in query.split('&') {
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        if k == key {
            return Some(v.to_owned());
        }
    }
    None
}

/// Parse a `since` query field as either a unix timestamp
/// (`1700000000`) or an ISO calendar date (`YYYY-MM-DD`,
/// midnight UTC). Returns `None` when the field is absent or
/// malformed; the dashboard's `/api/aggregates` endpoint falls
/// back to "7 days ago" in that case. Mirrors the
/// `parse_since` helper used by `moagan telemetry cleanup`.
fn parse_query_since(query: &str, key: &str) -> Option<i64> {
    let raw = parse_query_string(query, key)?;
    if let Ok(secs) = raw.parse::<i64>() {
        return Some(secs);
    }
    let date = chrono::NaiveDate::parse_from_str(&raw, "%Y-%m-%d").ok()?;
    let datetime = date.and_hms_opt(0, 0, 0)?;
    Some(datetime.and_utc().timestamp())
}

fn parse_query_export_level(query: &str) -> Result<ExportLevel> {
    let raw = parse_query_u64_or_str(query, "level").unwrap_or_else(|| "summary".into());
    raw.parse::<ExportLevel>()
}

fn parse_query_export_format(query: &str) -> Result<ExportFormat> {
    let raw = parse_query_u64_or_str(query, "format").unwrap_or_else(|| "tar.gz".into());
    raw.parse::<ExportFormat>()
}

fn parse_query_u64_or_str(query: &str, key: &str) -> Option<String> {
    for part in query.split('&') {
        let (k, v) = part.split_once('=')?;
        if k == key {
            return Some(v.to_owned());
        }
    }
    None
}

async fn read_request(stream: &mut TcpStream) -> std::result::Result<ParsedRequest, u16> {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let mut total = 0usize;
    let request_line = loop {
        let mut line = Vec::new();
        loop {
            let byte = match reader.read_u8().await {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(400),
                Err(_) => return Err(400),
            };
            if byte == b'\n' {
                break;
            }
            line.push(byte);
            if line.len() > MAX_HEADER_BYTES {
                return Err(431);
            }
        }
        total += line.len() + 1;
        if total > MAX_HEADER_BYTES {
            return Err(431);
        }
        if line.is_empty() {
            continue;
        }
        if line.starts_with(b"GET ") || line.starts_with(b"POST ") {
            break String::from_utf8_lossy(&line).into_owned();
        }
        // discard pre-amble garbage; bail if too much
        if buf.len() + line.len() + 1 > MAX_HEADER_BYTES {
            return Err(431);
        }
        buf.extend_from_slice(&line);
        buf.push(b'\n');
        if buf.len() > 256 {
            return Err(400);
        }
    };
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(400);
    }
    let method = parts[0].to_owned();
    let target = parts[1].to_owned();
    if !target.starts_with('/') || target.starts_with("//") {
        return Err(400);
    }
    let version = parts[2].to_owned();
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(505);
    }
    // Drain remaining headers.
    loop {
        let mut line = Vec::new();
        loop {
            let byte = match reader.read_u8().await {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(_) => return Err(400),
            };
            if byte == b'\n' {
                break;
            }
            line.push(byte);
            if line.len() > MAX_HEADER_BYTES {
                return Err(431);
            }
        }
        if line.iter().all(|b| *b == b'\r' || *b == 0) || line.is_empty() {
            break;
        }
    }
    Ok(ParsedRequest {
        method,
        target,
        version,
    })
}

async fn write_response(stream: &mut TcpStream, version: &str, resp: &Response) -> Result<()> {
    let reason = reason_phrase(resp.status);
    tracing::trace!(
        status = resp.status,
        body_len = resp.body.len(),
        "dashboard::write_response"
    );
    stream
        .write_all(
            format!(
                "{version} {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                resp.status,
                resp.content_type,
                resp.body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.write_all(&resp.body).await?;
    stream.flush().await?;
    Ok(())
}

async fn write_error(stream: &mut TcpStream, status: u16, message: &str) -> Result<()> {
    tracing::debug!(status, message, "dashboard::write_error");
    let body = format!("{status} {message}\n");
    let reason = reason_phrase(status);
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

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

// Borrow-checker hint: silence unused warnings for the read-once
// helpers below (kept around for the inline-document feel of the
// proxy implementation, which the dashboard mirrors).
#[allow(dead_code)]
fn _read_helpers_unused_marker(_io: &io::Result<()>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::PhaseSummaryRow;
    use crate::time::now_unix_secs;
    use std::net::IpAddr;

    fn stub_cfg(tmp: &tempfile::TempDir) -> DashboardConfig {
        // Use `MoaganHome::at(path)` directly so the test does not
        // mutate the global `MOAGAN_HOME` env var (which is
        // shared with other tests running in parallel).
        let home = Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
        DashboardConfig {
            bind: SocketAddr::new(IpAddr::V4("127.0.0.1".parse().unwrap()), 0),
            home,
            db_path: None,
        }
    }

    fn seed_run(tmp: &tempfile::TempDir, id: RunId) {
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_dir = home.run_dir(id);
        run_dir.ensure().unwrap();
        std::fs::write(run_dir.manifest(), b"{}").unwrap();
        std::fs::write(run_dir.proposals().join("p_01.json"), b"{\"p\":1}").unwrap();
    }

    #[tokio::test]
    async fn port_search_skips_blacklist() {
        assert_eq!(pick_port(80), 81, "blacklist[80] -> 81");
        assert_eq!(pick_port(443), 444, "blacklist[443] -> 444");
        assert_eq!(pick_port(4096), 4096);
    }

    #[tokio::test]
    async fn dispatch_runs_returns_empty_array_when_no_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let resp = dispatch("/api/runs", "", &cfg).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"[]");
    }

    #[tokio::test]
    async fn dispatch_runs_includes_seeded_run() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let id = RunId::new();
        seed_run(&tmp, id);
        // Register the run in SQLite so list_runs picks it up.
        let db = open_db(&cfg).unwrap();
        db.register_run(id, "fast", "completed", "0.3.0", None, None, None)
            .unwrap();
        let resp = dispatch("/api/runs", "", &cfg).await.unwrap();
        assert_eq!(resp.status, 200);
        let body = std::str::from_utf8(&resp.body).unwrap();
        assert!(body.contains(&id.to_string()));
    }

    #[tokio::test]
    async fn dispatch_unknown_run_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let id = RunId::new();
        let resp = dispatch(&format!("/api/runs/{id}"), "", &cfg)
            .await
            .unwrap_err();
        assert_eq!(resp.0, 404);
    }

    #[tokio::test]
    async fn dispatch_invalid_run_id_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let resp = dispatch("/api/runs/not-a-uuid", "", &cfg)
            .await
            .unwrap_err();
        assert_eq!(resp.0, 400);
    }

    #[tokio::test]
    async fn dispatch_phases_endpoint_returns_array() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let id = RunId::new();
        seed_run(&tmp, id);
        let db = open_db(&cfg).unwrap();
        db.register_run(id, "fast", "completed", "0.3.0", None, None, None)
            .unwrap();
        db.record_phase(id, "intake", 0, "start", None).unwrap();
        db.record_phase(id, "intake", 0, "end", None).unwrap();
        let resp = dispatch(&format!("/api/runs/{id}/phases"), "", &cfg)
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let body: Vec<PhaseSummaryRow> = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].phase, "intake");
        assert_eq!(body[0].status, "end");
    }

    #[tokio::test]
    async fn dispatch_hashes_returns_file_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let id = RunId::new();
        seed_run(&tmp, id);
        let resp = dispatch(&format!("/api/runs/{id}/hashes"), "", &cfg)
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let rows: Vec<HashRow> = serde_json::from_slice(&resp.body).unwrap();
        assert!(rows.iter().any(|r| r.path == "manifest.json"));
        assert!(rows.iter().any(|r| r.path == "proposals/p_01.json"));
    }

    #[tokio::test]
    async fn dispatch_run_detail_returns_combined_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let id = RunId::new();
        seed_run(&tmp, id);
        let db = open_db(&cfg).unwrap();
        db.register_run(id, "fast", "completed", "0.3.0", None, None, None)
            .unwrap();
        let resp = dispatch(&format!("/api/runs/{id}"), "", &cfg)
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v.get("run").is_some());
        assert!(v.get("aggregate").is_some());
        assert!(v.get("phases").is_some());
        assert!(v.get("provider_usage").is_some());
    }

    #[tokio::test]
    async fn dispatch_root_returns_index() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let resp = dispatch("/", "", &cfg).await.unwrap();
        assert_eq!(resp.status, 200);
        let body = std::str::from_utf8(&resp.body).unwrap();
        assert!(body.contains("/api/runs"));
    }

    #[tokio::test]
    async fn dispatch_export_endpoint_bundles_tar_gz() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let id = RunId::new();
        seed_run(&tmp, id);
        let resp = dispatch(
            &format!("/api/runs/{id}/export"),
            "level=summary&format=tar.gz",
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        // The endpoint must stream a valid gzip stream, not a
        // JSON summary. The first two bytes are the gzip magic
        // 0x1f 0x8b; the body must be parseable as a non-empty
        // tarball.
        assert_eq!(resp.content_type, "application/gzip");
        assert!(
            resp.body.len() > 2,
            "gzip body too small: {} bytes",
            resp.body.len()
        );
        assert_eq!(&resp.body[..2], &[0x1f, 0x8b], "missing gzip magic");
    }

    #[tokio::test]
    async fn dispatch_export_info_endpoint_returns_json() {
        // Backwards-compat: the new `/export-info` endpoint
        // preserves the old JSON shape for callers that only want
        // the summary fields (file_count, archive_sha256, ...).
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let id = RunId::new();
        seed_run(&tmp, id);
        let resp = dispatch(
            &format!("/api/runs/{id}/export-info"),
            "level=summary&format=tar.gz",
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "application/json; charset=utf-8");
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v["file_count"].as_u64().unwrap() >= 2);
        assert_eq!(v["archive_sha256"].as_str().unwrap().len(), 64);
    }

    #[tokio::test]
    async fn dispatch_export_unknown_format_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let id = RunId::new();
        seed_run(&tmp, id);
        let resp = dispatch(
            &format!("/api/runs/{id}/export"),
            "level=summary&format=rar",
            &cfg,
        )
        .await
        .unwrap_err();
        assert_eq!(resp.0, 400);
    }

    #[tokio::test]
    async fn rejects_non_loopback_bind() {
        let cfg = DashboardConfig {
            bind: SocketAddr::new(IpAddr::V4("0.0.0.0".parse().unwrap()), 4096),
            home: Arc::new(MoaganHome::resolve().unwrap()),
            db_path: None,
        };
        let res = start(cfg).await;
        let err = res.unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn end_to_end_get_runs_returns_200() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let handle = start(cfg).await.unwrap();
        let port = handle.local_addr.port();
        let id = RunId::new();
        seed_run(&tmp, id);
        let db = open_db(&stub_cfg(&tmp)).unwrap();
        db.register_run(id, "fast", "completed", "0.3.0", None, None, None)
            .unwrap();
        // Issue a real HTTP/1.1 GET against the bound port.
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /api/runs HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK"));
        assert!(text.contains(&id.to_string()));
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn end_to_end_post_returns_405() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let handle = start(cfg).await.unwrap();
        let port = handle.local_addr.port();
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(b"POST /api/runs HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        assert!(text.starts_with("HTTP/1.1 405"), "got: {text}");
        handle.shutdown().await.unwrap();
    }

    #[test]
    fn port_blacklist_contains_well_known_services() {
        assert!(PORT_BLACKLIST.contains(&22));
        assert!(PORT_BLACKLIST.contains(&443));
        assert!(PORT_BLACKLIST.contains(&3306));
    }

    #[test]
    fn hash_rows_sort_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b"), b"b").unwrap();
        std::fs::write(tmp.path().join("a"), b"a").unwrap();
        let rows = compute_hashes(tmp.path()).unwrap();
        assert_eq!(rows[0].path, "a");
        assert_eq!(rows[1].path, "b");
    }

    #[test]
    fn now_unix_secs_is_recent() {
        // Sanity: now_unix_secs() returns a timestamp near the
        // build time. Used by retention tests, but tested here
        // to keep the smoke surface small.
        let now = now_unix_secs();
        assert!(now > 1_700_000_000, "now must be after 2023-11-14");
    }

    /// J#5 closure: with no runs registered the `/api/lineage`
    /// endpoint returns the JSON for an empty [`LineageGraph`]
    /// (`{"nodes":[],"edges":[]}`), not a 404. Callers depend on
    /// the empty graph as a stable first response for a fresh
    /// install.
    #[tokio::test]
    async fn dashboard_lineage_returns_empty_for_no_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let resp = dispatch("/api/lineage", "", &cfg).await.unwrap();
        assert_eq!(resp.status, 200);
        let graph: LineageGraph =
            serde_json::from_str(std::str::from_utf8(&resp.body).unwrap()).unwrap();
        assert!(graph.nodes.is_empty(), "no runs => no nodes");
        assert!(graph.edges.is_empty(), "no runs => no edges");
    }

    /// J#5 closure: seeded `parent_run_id` <-> `run_id` edges
    /// surface on `/api/lineage`. Root nodes (no parent) only
    /// show up when they appear in a recorded parent slot, so a
    /// run with `parent_run_id = NULL` is invisible until at
    /// least one child claims it. The test uses two generations
    /// (parent -> child -> grandchild) to exercise the
    /// deduplication path in [`LineageGraph::from_pairs`].
    #[tokio::test]
    async fn dashboard_lineage_returns_edges_for_parent_child() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let db = open_db(&cfg).unwrap();
        let parent = RunId::new();
        let child = RunId::new();
        let grandchild = RunId::new();
        db.register_run(parent, "fast", "completed", "0.3.0", None, None, None)
            .unwrap();
        db.register_run(
            child,
            "fast",
            "completed",
            "0.3.0",
            None,
            None,
            Some(parent),
        )
        .unwrap();
        db.register_run(
            grandchild,
            "fast",
            "completed",
            "0.3.0",
            None,
            None,
            Some(child),
        )
        .unwrap();
        let resp = dispatch("/api/lineage", "", &cfg).await.unwrap();
        assert_eq!(resp.status, 200);
        let graph: LineageGraph =
            serde_json::from_str(std::str::from_utf8(&resp.body).unwrap()).unwrap();
        // Three distinct nodes, two edges.
        assert_eq!(graph.nodes.len(), 3, "expected 3 distinct nodes");
        assert_eq!(graph.edges.len(), 2, "expected 2 edges");
        // Order is created_unix DESC; multiple inserts in the same
        // wall-clock second share an order with the underlying
        // rowid, so we sort both sides before comparing.
        let mut expected_edges: Vec<(String, String)> = vec![
            (parent.to_string(), child.to_string()),
            (child.to_string(), grandchild.to_string()),
        ];
        let mut actual_edges: Vec<(String, String)> = graph.edges.clone();
        expected_edges.sort();
        actual_edges.sort();
        assert_eq!(actual_edges, expected_edges);
        // The same dedup-via-HashMap contract is shared with the
        // lineage_graph unit tests; pin the node set membership too.
        let mut expected_nodes = vec![
            parent.to_string(),
            child.to_string(),
            grandchild.to_string(),
        ];
        let mut actual_nodes = graph.nodes.clone();
        expected_nodes.sort();
        actual_nodes.sort();
        assert_eq!(actual_nodes, expected_nodes);
    }

    // -----------------------------------------------------------------
    // Cross-run analytics endpoints (D.17 dashboard extension).
    //
    // Each test seeds 2–3 runs through the SQLite index, hits
    // `dispatch(...)` directly, and asserts on the JSON body the
    // dashboard SPA would consume. The unit tests for the
    // `compare_runs` / `aggregates_window` helpers live next to
    // the implementations in `src/storage/sqlite.rs`.
    // -----------------------------------------------------------------

    /// Seed one call row for `run_id`. Mirrors the
    /// `src/storage/sqlite.rs::tests::seed_call` helper but
    /// keeps the dashboard tests self-contained so a
    /// future refactor of the test helper does not cascade
    /// through the dashboard module.
    #[allow(clippy::too_many_arguments)]
    fn seed_dashboard_call(
        db: &crate::storage::sqlite::Db,
        call_id: &str,
        run_id: RunId,
        phase: &str,
        provider: &str,
        model: &str,
        http_status: Option<u16>,
        input_tokens: u64,
        output_tokens: u64,
        started_unix: i64,
        ended_unix: i64,
        error: Option<&str>,
    ) {
        db.record_call(
            call_id,
            run_id,
            phase,
            phase,
            provider,
            model,
            &format!("ck-{call_id}"),
            Some(&format!("sha-{call_id}")),
            false,
            http_status.map(i64::from),
            input_tokens,
            output_tokens,
            0,
            0,
            started_unix,
            ended_unix,
            error,
            0,
        )
        .unwrap();
    }

    /// Backdate `runs.created_unix` / `runs.updated_unix` so a
    /// test can pin `avg_duration_secs` without racing the
    /// wall clock. Internal helper used only by the
    /// cross-run aggregate tests below.
    fn backdate_run(
        db: &crate::storage::sqlite::Db,
        run_id: RunId,
        created_unix: i64,
        updated_unix: i64,
    ) {
        let conn = db.connection().unwrap();
        conn.execute(
            "UPDATE runs SET created_unix = ?, updated_unix = ? WHERE run_id = ?",
            rusqlite::params![created_unix, updated_unix, run_id.to_string()],
        )
        .unwrap();
    }

    /// /api/compare-runs returns the diff for two seeded runs:
    /// the response carries one entry per requested id (in the
    /// input order), the shared provider intersection, and the
    /// cross-run diff.
    #[tokio::test]
    async fn dashboard_compare_runs_returns_diff_for_two_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let db = open_db(&cfg).unwrap();
        let a = RunId::new();
        let b = RunId::new();
        db.register_run(a, "fast", "completed", "0.9.1", None, None, None)
            .unwrap();
        db.register_run(b, "fast", "completed", "0.9.1", None, None, None)
            .unwrap();
        backdate_run(&db, a, 1_000, 1_100);
        backdate_run(&db, b, 2_000, 2_250);
        seed_dashboard_call(
            &db,
            "a1",
            a,
            "intake",
            "minimax",
            "M3",
            Some(200),
            100,
            50,
            10,
            11,
            None,
        );
        seed_dashboard_call(
            &db,
            "a2",
            a,
            "intake",
            "minimax",
            "M3",
            Some(500),
            0,
            0,
            20,
            21,
            Some("server error"),
        );
        seed_dashboard_call(
            &db,
            "b1",
            b,
            "intake",
            "minimax",
            "M3",
            Some(200),
            200,
            80,
            30,
            31,
            None,
        );
        seed_dashboard_call(
            &db,
            "b2",
            b,
            "rank",
            "opencode",
            "gpt-4o",
            Some(200),
            300,
            120,
            40,
            41,
            None,
        );

        let query = format!("ids={},{}", a, b);
        let resp = dispatch("/api/compare-runs", &query, &cfg).await.unwrap();
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["runs"].as_array().unwrap().len(), 2);
        assert_eq!(v["shared_providers"], serde_json::json!(["minimax"]));
        assert_eq!(v["diff"]["max_duration_secs"], 250);
        assert_eq!(v["diff"]["min_duration_secs"], 100);
        assert_eq!(v["diff"]["max_error_calls"], 1);
        // Run A: 100+50=150 (minimax). Run B: 200+80=280
        // (minimax) + 300+120=420 (opencode). minimax union
        // = 150 + 280 = 430.
        assert_eq!(v["diff"]["provider_token_total"]["minimax"], 430);
        assert_eq!(v["diff"]["provider_token_total"]["opencode"], 420);
    }

    /// /api/compare-runs returns 400 when fewer than two ids
    /// are supplied (the SQLite helper stays safe with `< 2`
    /// but the dashboard surfaces the early 400).
    #[tokio::test]
    async fn dashboard_compare_runs_too_few_ids_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let only = RunId::new();
        let resp = dispatch("/api/compare-runs", &format!("ids={only}"), &cfg)
            .await
            .unwrap_err();
        assert_eq!(resp.0, 400);
        assert!(resp.1.contains("at least 2"));
    }

    /// /api/compare-runs returns 400 when one of the ids is
    /// not a valid UUID.
    #[tokio::test]
    async fn dashboard_compare_runs_invalid_uuid_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let valid = RunId::new();
        let resp = dispatch(
            "/api/compare-runs",
            &format!("ids={valid},not-a-uuid"),
            &cfg,
        )
        .await
        .unwrap_err();
        assert_eq!(resp.0, 400);
        assert!(resp.1.contains("invalid run id"));
    }

    /// /api/compare-runs returns 404 when any of the ids does
    /// not exist in the `runs` table. The dashboard catches
    /// the missing id BEFORE delegating to the SQLite helper
    /// so the operator gets a precise 404 with the missing
    /// run id in the message.
    #[tokio::test]
    async fn dashboard_compare_runs_missing_id_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let db = open_db(&cfg).unwrap();
        let a = RunId::new();
        db.register_run(a, "fast", "completed", "0.9.1", None, None, None)
            .unwrap();
        let missing = RunId::new(); // not registered
        let resp = dispatch("/api/compare-runs", &format!("ids={a},{missing}"), &cfg)
            .await
            .unwrap_err();
        assert_eq!(resp.0, 404);
        assert!(resp.1.contains(&missing.to_string()));
    }

    /// /api/aggregates (no filters) defaults to a 7-day window
    /// starting from `now - 7 * 86_400`. With no seeded
    /// calls the response carries zero counters and a
    /// `provider: null` field.
    #[tokio::test]
    async fn dashboard_aggregates_default_window_returns_zero_counters() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let before = crate::time::now_unix_secs();
        let resp = dispatch("/api/aggregates", "", &cfg).await.unwrap();
        let after = crate::time::now_unix_secs();
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["total_runs"], 0);
        assert_eq!(v["total_calls"], 0);
        assert_eq!(v["total_tokens"], 0);
        assert_eq!(v["p50_latency_ms"], 0.0);
        assert_eq!(v["p95_latency_ms"], 0.0);
        assert_eq!(v["p99_latency_ms"], 0.0);
        assert_eq!(v["error_rate"], 0.0);
        assert!(v["provider"].is_null());
        // The default `since_unix` is `now - 7 days`. The
        // handler snapshots `now` once when it processes the
        // request; pin the lower bound at `before - 7*86400`
        // and the upper bound at `after - 7*86400` so a tiny
        // wall-clock drift between the snapshot and the test
        // does not flake.
        let since = v["since_unix"].as_i64().unwrap();
        let seven_days = 7_i64 * 86_400;
        assert!(
            since >= before - seven_days - 1 && since <= after - seven_days + 1,
            "since_unix should land within 1s of now-7d; got before={before} after={after} since={since}"
        );
    }

    /// /api/aggregates with `since=YYYY-MM-DD` parses the date
    /// at midnight UTC and counts every run whose
    /// `created_unix >= since`.
    #[tokio::test]
    async fn dashboard_aggregates_since_date_picks_up_seeded_run() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let db = open_db(&cfg).unwrap();
        let run = RunId::new();
        db.register_run(run, "fast", "completed", "0.9.1", None, None, None)
            .unwrap();
        // Backdate to 2026-01-15 (unix 1_768_483_200).
        let created = 1_768_483_200_i64;
        backdate_run(&db, run, created, created + 100);
        seed_dashboard_call(
            &db,
            "c1",
            run,
            "intake",
            "minimax",
            "M3",
            Some(200),
            50,
            25,
            created + 1,
            created + 2,
            None,
        );

        let resp = dispatch("/api/aggregates", "since=2026-01-01", &cfg)
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["total_runs"], 1);
        assert_eq!(v["total_calls"], 1);
        assert_eq!(v["total_tokens"], 75);
        assert_eq!(
            v["since_unix"],
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .timestamp()
        );
    }

    /// /api/aggregates with `provider=NAME` narrows the
    /// `total_calls` and `total_runs` to the matching slice.
    #[tokio::test]
    async fn dashboard_aggregates_provider_filter_narrows_results() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = stub_cfg(&tmp);
        let db = open_db(&cfg).unwrap();
        let run = RunId::new();
        db.register_run(run, "fast", "completed", "0.9.1", None, None, None)
            .unwrap();
        backdate_run(&db, run, 1_000, 1_050);
        seed_dashboard_call(
            &db,
            "m1",
            run,
            "intake",
            "minimax",
            "M3",
            Some(200),
            10,
            5,
            1_000,
            1_001,
            None,
        );
        seed_dashboard_call(
            &db,
            "o1",
            run,
            "intake",
            "opencode",
            "gpt-4o",
            Some(200),
            20,
            10,
            1_002,
            1_003,
            None,
        );
        let resp = dispatch("/api/aggregates", "since=1970-01-01&provider=minimax", &cfg)
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["provider"], "minimax");
        assert_eq!(v["total_calls"], 1);
        assert_eq!(v["total_tokens"], 15);
    }

    /// `parse_query_csv` is the parser the dashboard uses for
    /// `?ids=a,b,c`; pinning its behaviour guards against an
    /// off-by-one in the trimming / empty-segment filter.
    #[test]
    fn dashboard_parse_query_csv_handles_edge_cases() {
        // Two ids, normal case.
        assert_eq!(
            parse_query_csv("ids=a,b", "ids"),
            vec!["a".to_string(), "b".to_string()]
        );
        // Trailing comma must not produce an empty segment
        // (operators frequently leave a trailing comma when
        // scripting).
        assert_eq!(
            parse_query_csv("ids=a,b,", "ids"),
            vec!["a".to_string(), "b".to_string()],
            "trailing comma must collapse to a single segment"
        );
        // Whitespace around each id is trimmed (operators
        // sometimes paste with surrounding spaces).
        assert_eq!(
            parse_query_csv("ids= a , b ", "ids"),
            vec!["a".to_string(), "b".to_string()],
            "leading/trailing whitespace must be trimmed"
        );
        // Absent key returns empty vec.
        assert!(parse_query_csv("limit=10", "ids").is_empty());
    }

    /// `parse_query_since` accepts both unix timestamps and
    /// ISO calendar dates. The dashboard relies on the
    /// calendar branch for human-readable URLs.
    #[test]
    fn dashboard_parse_query_since_accepts_unix_and_iso() {
        assert_eq!(parse_query_since("", "since"), None);
        assert_eq!(
            parse_query_since("since=1700000000", "since"),
            Some(1_700_000_000)
        );
        let iso = parse_query_since("since=2026-01-15", "since").unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(iso, expected);
        // Garbage returns None so the dashboard falls back
        // to the 7-day default.
        assert_eq!(parse_query_since("since=not-a-date", "since"), None);
    }
}
