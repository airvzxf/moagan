//! `moagan audit` subcommands: `proxy` (the sidecar) and `verify`
//! (the cross-checker). The sidecar lives in a separate process so
//! that even if the main `moagan run` process hangs or crashes, the
//! on-disk JSONL keeps growing with every request/response that
//! crossed the loopback.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal;

use crate::audit::proxy::{self, ProxyConfig};
use crate::audit::verify as verify_mod;
use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;

/// `moagan audit proxy` — bind 127.0.0.1:<port>, forward to
/// `--upstream`, append every request/response to
/// `<run_dir>/telemetry/external_audit.jsonl`. SIGINT/SIGTERM
/// triggers a clean shutdown.
#[derive(Debug, Clone)]
pub struct ProxyArgs {
    /// Override MOAGAN_HOME.
    pub runs_dir: Option<PathBuf>,
    /// Target run id. Defaults to the most recent run.
    pub run_id: Option<String>,
    /// Bind host (default 127.0.0.1).
    pub listen_host: String,
    /// Bind port. `0` requests a kernel-assigned free port.
    pub port: u16,
    /// Upstream base URL (e.g. `https://api.minimax.io/anthropic/v1`).
    pub upstream: String,
    /// Default `false` (include bodies). Pass `--exclude-bodies` to
    /// drop `body_canonical` from the log and keep only `body_sha256`.
    pub exclude_bodies: bool,
    /// Hard cap on request body size in bytes.
    pub max_body_bytes: usize,
    /// Upstream HTTP timeout in seconds.
    pub timeout_secs: u64,
}

/// `moagan audit verify` — cross-check the sidecar JSONL against
/// Moagan's internal `calls.jsonl.gz` + SQLite, write a TSV summary,
/// return the exit code (0 ok, 1 mismatch/orphans, 2 missing/invalid).
#[derive(Debug, Clone)]
pub struct VerifyArgs {
    /// Override MOAGAN_HOME.
    pub runs_dir: Option<PathBuf>,
    /// Target run id. Defaults to the most recent run.
    pub run_id: Option<String>,
}

/// Resolve the home and run id for an audit subcommand.
///
/// `require_run_id` controls what happens when `run_id` is `None`:
/// - `true`: error out if no run exists (used by `verify`).
/// - `false`: return `None` and let the proxy start in dynamic mode
///   so it discovers new runs as they appear (used by `proxy`).
pub fn resolve_run(
    args_runs_dir: Option<PathBuf>,
    run_id: Option<String>,
    require_run_id: bool,
) -> Result<(Arc<MoaganHome>, Option<RunId>)> {
    if let Some(ref home) = args_runs_dir {
        unsafe {
            std::env::set_var("MOAGAN_HOME", home);
        }
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;
    let run_id = match run_id {
        Some(id) => Some(id.parse().map_err(|e| Error::InvalidArgs(format!("{e}")))?),
        None if require_run_id => Some(pick_latest_run(&home)?),
        None => None,
    };
    Ok((home, run_id))
}

fn pick_latest_run(home: &MoaganHome) -> Result<RunId> {
    let runs_dir = home.runs_dir();
    let mut entries: Vec<_> = std::fs::read_dir(&runs_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.parse::<RunId>().ok().map(|id| (id, e.path()))
        })
        .collect();
    entries.sort_by_key(|b| std::cmp::Reverse(b.0));
    entries
        .into_iter()
        .next()
        .map(|(id, _)| id)
        .ok_or_else(|| Error::InvalidState("no runs found in MOAGAN_HOME/.runs".into()))
}

/// `moagan audit proxy` — bind 127.0.0.1:<port>, forward to
/// `--upstream`, append every request/response to
/// `<run_dir>/telemetry/external_audit.jsonl`. SIGINT/SIGTERM
/// triggers a clean shutdown.
pub async fn proxy_cmd(args: ProxyArgs) -> Result<()> {
    let (home, run_id) = resolve_run(args.runs_dir.clone(), args.run_id.clone(), false)?;
    let listen = match (args.listen_host.as_str(), args.port) {
        (host, 0) => format!("{host}:0")
            .parse()
            .map_err(|e| Error::InvalidArgs(format!("bind: {e}")))?,
        (host, p) => format!("{host}:{p}")
            .parse()
            .map_err(|e| Error::InvalidArgs(format!("bind: {e}")))?,
    };
    let cfg = ProxyConfig {
        listen,
        upstream: args.upstream.clone(),
        runs_dir: home.root().to_path_buf(),
        run_id,
        include_bodies: !args.exclude_bodies,
        upstream_timeout: Duration::from_secs(args.timeout_secs),
        max_body_bytes: args.max_body_bytes,
        refuse_loopback_forward: false,
        refuse_loopback_forward_allowed: true,
        fixed_log_path: None,
    };
    let handle = proxy::start(cfg).await?;
    let run_label = match run_id {
        Some(id) => id.short(),
        None => "auto".to_owned(),
    };
    eprintln!(
        "proxy listening on http://{} -> {}\nrun id: {}\nruns dir: {}",
        handle.local_addr,
        args.upstream,
        run_label,
        home.root().display()
    );
    #[cfg(unix)]
    {
        let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c().await?;
    }
    handle.shutdown().await;
    eprintln!(
        "proxy shut down; audit logs under {}/.runs/",
        home.root().display()
    );
    Ok(())
}

/// `moagan audit verify` — cross-check the sidecar JSONL against
/// Moagan's internal `calls.jsonl.gz` + SQLite, write a TSV summary,
/// return the exit code (0 ok, 1 mismatch/orphans, 2 missing/invalid).
pub async fn verify_cmd(args: VerifyArgs) -> Result<i32> {
    let (home, run_id) = resolve_run(args.runs_dir.clone(), args.run_id.clone(), true)?;
    let run_id = run_id.expect("resolve_run(require_run_id=true) returned None");
    let run_dir = home.run_dir(run_id);
    let calls_path = run_dir.telemetry().join("calls.jsonl.gz");
    let report = verify_mod::verify(&run_dir, &calls_path)?;
    let tsv_path = run_dir.external_audit_verify_path();
    verify_mod::write_tsv(&report, &tsv_path)?;
    println!("metric\tvalue");
    println!("match_count\t{}", report.match_count);
    println!("body_mismatch_count\t{}", report.body_mismatch_count);
    println!("orphan_request_count\t{}", report.orphan_request_count);
    println!("orphan_response_count\t{}", report.orphan_response_count);
    println!(
        "unmatched_internal_count\t{}",
        report.unmatched_internal_count
    );
    println!(
        "unmatched_external_count\t{}",
        report.unmatched_external_count
    );
    println!("crc_invalid_count\t{}", report.crc_invalid_count);
    println!("summary\t{}", report.summary());
    if !tsv_path.as_os_str().is_empty() {
        eprintln!("tsv: {}", tsv_path.display());
    }
    Ok(report.exit_code())
}
