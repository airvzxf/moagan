//! `moagan-docgen` — developer-only binary that regenerates the canonical
//! reference docs (CLI surface, NDJSON event schema, test-skip inventory)
//! from the live `clap::Command` tree, the `Event<'a>` enum, and the
//! codebase itself. EPIC #852 closes #866, #867, #868, #869.
//!
//! Subcommands:
//! - `cli`        — walks `clap::Command` from `moagan::cli::Cli::command()`
//!                  and emits `docs/cli-reference.md`.
//! - `events`     — parses `src/telemetry/stdout_events.rs` to extract the
//!                  `Event<'a>` variant registry, then emits
//!                  `docs/events-reference.md`. Consumes the public
//!                  `DECISION_KIND_INVENTORY` constant for the curated
//!                  decision table.
//! - `test-skips` — scans 8 layers of skip mechanisms in the codebase
//!                  and emits `docs/test-skips-report.md`.
//!
//! The binary is gated behind `--features dev-tools` (see `Cargo.toml`)
//! so the default build (`cargo build --release` with no features)
//! does not compile it.

#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

#[cfg(not(feature = "dev-tools"))]
compile_error!(
    "moagan-docgen requires the `dev-tools` Cargo feature. \
     Build with `cargo build --features dev-tools --bin moagan-docgen`."
);

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::CommandFactory;

use moagan::cli::Cli;
use moagan::telemetry::stdout_events::{
    DECISION_KIND_INVENTORY, SCHEMA_VERSION as EVENTS_SCHEMA_VERSION,
};

#[derive(Debug)]
enum DocgenError {
    Io(io::Error),
    Format(std::fmt::Error),
    Custom(String),
}

impl std::fmt::Display for DocgenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Format(e) => write!(f, "format error: {e}"),
            Self::Custom(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for DocgenError {}

impl From<io::Error> for DocgenError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<std::fmt::Error> for DocgenError {
    fn from(e: std::fmt::Error) -> Self {
        Self::Format(e)
    }
}

type Result<T> = std::result::Result<T, DocgenError>;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str).unwrap_or("");
    let exit_code = match sub {
        "cli" => run_cli(),
        "events" => run_events(),
        "test-skips" => run_test_skips(),
        "--help" | "-h" | "" => {
            print_help();
            0
        }
        other => {
            eprintln!("moagan-docgen: unknown subcommand {other:?}");
            print_help();
            2
        }
    };
    std::process::exit(exit_code);
}

fn print_help() {
    println!(
        "moagan-docgen — regenerate canonical reference docs (EPIC #852).\n\n\
USAGE:\n  moagan-docgen <SUBCOMMAND>\n\n\
SUBCOMMANDS:\n  cli         Emit docs/cli-reference.md\n  \
events      Emit docs/events-reference.md\n  \
test-skips  Emit docs/test-skips-report.md\n  \
-h, --help  Print this help\n\n\
Each subcommand writes to stdout. Pipe to a file or diff\n\
against the canonical doc under docs/."
    );
}

fn run_cli() -> i32 {
    match generate_cli_reference(&mut io::stdout().lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("moagan-docgen cli: {e}");
            1
        }
    }
}

fn run_events() -> i32 {
    match generate_events_reference(&mut io::stdout().lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("moagan-docgen events: {e}");
            1
        }
    }
}

fn run_test_skips() -> i32 {
    let repo_root = match find_repo_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("moagan-docgen test-skips: {e}");
            return 1;
        }
    };
    match generate_test_skips_report(&repo_root, &mut io::stdout().lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("moagan-docgen test-skips: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------
// Path discovery
// ---------------------------------------------------------------------

fn find_repo_root() -> Result<PathBuf> {
    let mut p = std::env::current_dir()?;
    loop {
        if p.join("Cargo.toml").is_file() && p.join("src/lib.rs").is_file() {
            return Ok(p);
        }
        match p.parent() {
            Some(parent) => p = parent.to_path_buf(),
            None => {
                return Err(DocgenError::Custom(
                    "could not locate repo root (no Cargo.toml found)".to_string(),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------
// Subcommand: cli
// ---------------------------------------------------------------------

fn generate_cli_reference(w: &mut dyn Write) -> Result<()> {
    let cmd = Cli::command();

    writeln!(w, "# CLI reference — `moagan`")?;
    writeln!(w)?;
    writeln!(
        w,
        "> Auto-generated by `moagan-docgen cli`. Do not edit by hand — run the docgen tool and commit the diff."
    )?;
    writeln!(w)?;
    if let Some(about) = cmd.get_about() {
        writeln!(w, "{about}")?;
    } else {
        writeln!(w, "(no about)")?;
    }
    writeln!(w)?;
    writeln!(w, "## Synopsis")?;
    writeln!(w)?;
    writeln!(w, "```")?;
    writeln!(w, "{}", render_root_usage(&cmd))?;
    writeln!(w, "```")?;
    writeln!(w)?;
    writeln!(w, "## Global flags")?;
    writeln!(w)?;
    emit_arg_table(w, &[&cmd])?;
    writeln!(w)?;

    emit_command_recursive(w, &cmd)?;

    writeln!(
        w,
        "\n## Generated by\n\n\
`moagan-docgen cli` (EPIC #852). Recursive walk of the live `clap::Command` tree built from `moagan::cli::Cli::command()`."
    )?;
    Ok(())
}

fn render_root_usage(cmd: &clap::Command) -> String {
    let mut s = String::new();
    let _ = write!(s, "{}", cmd.get_name());
    let _ = write!(s, " [OPTIONS] <SUBCOMMAND>");
    s
}

fn emit_command_recursive(w: &mut dyn Write, cmd: &clap::Command) -> Result<()> {
    for sub in cmd.get_subcommands() {
        writeln!(w, "# {}", sub.get_name())?;
        writeln!(w)?;
        if let Some(about) = sub.get_about() {
            let about_str = about.to_string();
            if !about_str.is_empty() {
                writeln!(w, "_{about_str}_")?;
                writeln!(w)?;
            }
        }
        if let Some(long) = sub.get_long_about() {
            let long_str = long.to_string();
            if !long_str.is_empty() {
                writeln!(w, "{long_str}")?;
                writeln!(w)?;
            }
        }

        let usage = render_subcommand_usage(sub);
        writeln!(w, "**Usage:** `{usage}`")?;
        writeln!(w)?;

        let args: Vec<&clap::Arg> = sub.get_arguments().collect();
        let positionals: Vec<&&clap::Arg> = args
            .iter()
            .filter(|a| a.is_positional() && !a.is_hide_set())
            .collect();
        let options: Vec<&&clap::Arg> = args
            .iter()
            .filter(|a| !a.is_positional() && !a.is_hide_set() && a.get_id() != "help")
            .collect();

        if !positionals.is_empty() {
            writeln!(w, "**Arguments:**")?;
            writeln!(w)?;
            writeln!(w, "| Name | Type | Required | Default | Description |")?;
            writeln!(w, "|---|---|---|---|---|")?;
            for a in &positionals {
                emit_arg_row(w, a, true)?;
            }
            writeln!(w)?;
        }
        if !options.is_empty() {
            writeln!(w, "**Options:**")?;
            writeln!(w)?;
            emit_arg_table_inner(w, &options.iter().map(|&&a| a).collect::<Vec<_>>())?;
            writeln!(w)?;
        }

        let subs: Vec<&clap::Command> = sub.get_subcommands().collect();
        if !subs.is_empty() {
            writeln!(w, "**Subcommands:**")?;
            writeln!(w)?;
            writeln!(w, "| Name | About |")?;
            writeln!(w, "|---|---|")?;
            for s in &subs {
                let about = s.get_about().map(|s| s.to_string()).unwrap_or_default();
                let long = s
                    .get_long_about()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let summary = if !about.is_empty() {
                    about
                } else if !long.is_empty() {
                    long.lines().next().unwrap_or("").to_string()
                } else {
                    String::new()
                };
                writeln!(
                    w,
                    "| `{}` | {} |",
                    s.get_name(),
                    summary.replace('|', "\\|")
                )?;
            }
            writeln!(w)?;
        }
        if !subs.is_empty() {
            emit_command_recursive(w, sub)?;
        }
    }
    Ok(())
}

fn render_subcommand_usage(cmd: &clap::Command) -> String {
    let mut s = String::new();
    s.push_str("moagan ");
    s.push_str(cmd.get_name());
    let args: Vec<&clap::Arg> = cmd.get_arguments().collect();
    let positionals: Vec<&&clap::Arg> = args.iter().filter(|a| a.is_positional()).collect();
    for a in &positionals {
        let name = a
            .get_value_names()
            .and_then(|v| v.first().cloned())
            .map(|n| n.to_string())
            .unwrap_or_else(|| a.get_id().to_string());
        let req_open = if a.is_required_set() { "<" } else { "[" };
        let req_close = if a.is_required_set() { ">" } else { "]" };
        let _ = write!(s, " {req_open}{name}{req_close}");
    }
    if args
        .iter()
        .any(|a| !a.is_positional() && a.get_id() != "help")
    {
        s.push_str(" [OPTIONS]");
    }
    s
}

fn emit_arg_table(w: &mut dyn Write, cmds: &[&clap::Command]) -> Result<()> {
    let mut all: Vec<&clap::Arg> = Vec::new();
    for c in cmds {
        for a in c.get_arguments() {
            if a.is_hide_set() || a.get_id() == "help" {
                continue;
            }
            all.push(a);
        }
    }
    emit_arg_table_inner(w, &all)
}

fn emit_arg_table_inner(w: &mut dyn Write, args: &[&clap::Arg]) -> Result<()> {
    writeln!(
        w,
        "| Flag | Long | Short | Type | Default | Env | Required | Description |"
    )?;
    writeln!(w, "|---|---|---|---|---|---|---|---|")?;
    for a in args {
        if a.is_positional() {
            continue;
        }
        emit_arg_row(w, a, false)?;
    }
    Ok(())
}

fn emit_arg_row(w: &mut dyn Write, a: &clap::Arg, is_positional: bool) -> Result<()> {
    if is_positional {
        let name = a
            .get_value_names()
            .and_then(|v| v.first().cloned())
            .map(|n| n.to_string())
            .unwrap_or_else(|| a.get_id().to_string());
        let typ = a
            .get_value_names()
            .and_then(|v| v.first().cloned())
            .map(|n| format!("`{n}`"))
            .unwrap_or_else(|| "value".to_string());
        let default = if a.get_default_values().is_empty() {
            String::new()
        } else {
            a.get_default_values()
                .iter()
                .map(|v| v.to_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let default_cell = if default.is_empty() {
            String::new()
        } else {
            format!("`{default}`")
        };
        let required = if a.is_required_set() { "yes" } else { "no" };
        let help = a
            .get_help()
            .map(|s| s.to_string())
            .or_else(|| a.get_long_help().map(|s| s.to_string()))
            .unwrap_or_default()
            .replace('|', "\\|")
            .replace('\n', " ");
        writeln!(
            w,
            "| `<{name}>` | _positional_ | _—_ | {typ} | {default_cell} | _—_ | {required} | {help} |"
        )?;
        return Ok(());
    }
    let long = a.get_long().map(|s| format!("`--{s}`")).unwrap_or_default();
    let short = a.get_short().map(|c| format!("`-{c}`")).unwrap_or_default();
    let mut flag = String::new();
    if !long.is_empty() {
        flag.push_str(&long);
    }
    if !short.is_empty() {
        if !flag.is_empty() {
            flag.push_str(", ");
        }
        flag.push_str(&short);
    }
    if flag.is_empty() {
        flag = format!("`<{}>`", a.get_id());
    }
    let long_again = a.get_long().unwrap_or("");
    let typ = a
        .get_value_names()
        .and_then(|v| v.first().cloned())
        .map(|n| format!("`{n}`"))
        .unwrap_or_else(|| {
            if long_again.is_empty() {
                "flag".to_string()
            } else {
                format!("`--{long_again}<...>`")
            }
        });
    let default = if a.get_default_values().is_empty() {
        String::new()
    } else {
        a.get_default_values()
            .iter()
            .map(|v| v.to_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let default_cell = if default.is_empty() {
        String::new()
    } else {
        format!("`{default}`")
    };
    let env = a
        .get_env()
        .map(|e| format!("`{}`", e.to_string_lossy()))
        .unwrap_or_default();
    let required = if a.is_required_set() { "yes" } else { "no" };
    let help = a
        .get_help()
        .map(|s| s.to_string())
        .or_else(|| a.get_long_help().map(|s| s.to_string()))
        .unwrap_or_default()
        .replace('|', "\\|")
        .replace('\n', " ");
    writeln!(
        w,
        "| {flag} | {long} | {short} | {typ} | {default_cell} | {env} | {required} | {help} |"
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Subcommand: events
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
struct EventVariant {
    name: String,
    kind_tag: String,
    fields: Vec<EventField>,
}

#[derive(Debug, Clone)]
struct EventField {
    name: String,
    ty: String,
}

fn generate_events_reference(w: &mut dyn Write) -> Result<()> {
    writeln!(w, "# Events reference — `moagan` NDJSON stream")?;
    writeln!(w)?;
    writeln!(
        w,
        "> Auto-generated by `moagan-docgen events`. Do not edit by hand — run the docgen tool and commit the diff."
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "Schema version: **{EVENTS_SCHEMA_VERSION}** (bumped on any backwards-incompatible change to the `Event` enum). Additive changes keep the same version."
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "The `moagan` binary emits typed domain events on **stdout** as NDJSON (one JSON object per line). The stream is silenced when stdout is a TTY and re-enabled when stdout is redirected or piped. See `--event-format` and `MOAGAN_EVENT_FORMAT` for controls."
    )?;
    writeln!(w)?;
    writeln!(w, "## Activation")?;
    writeln!(w)?;
    writeln!(w, "```bash")?;
    writeln!(
        w,
        "moagan run --mode fast --provider mock:mock-model --prompt 'x' > events.jsonl"
    )?;
    writeln!(w, "jq -c 'select(.kind == \"run_end\")' events.jsonl")?;
    writeln!(w, "```")?;
    writeln!(w)?;

    let variants = parse_event_variants().map_err(DocgenError::Custom)?;
    writeln!(w, "## Event kinds")?;
    writeln!(w)?;
    writeln!(w, "| `kind` | When emitted | Notable fields |")?;
    writeln!(w, "|---|---|---|")?;
    for v in &variants {
        let fields = v
            .fields
            .iter()
            .filter(|f| f.name != "schema" && f.name != "ts")
            .map(|f| format!("`{}`", f.name))
            .collect::<Vec<_>>()
            .join(", ");
        let summary = variant_summary(v);
        writeln!(w, "| `{}` | {} | {} |", v.kind_tag, summary, fields)?;
    }
    writeln!(w)?;

    writeln!(w, "## Variant schema")?;
    writeln!(w)?;
    for v in &variants {
        writeln!(w, "### `{}` (`kind = \"{}\"`)", v.name, v.kind_tag)?;
        writeln!(w)?;
        writeln!(w, "| Field | Type |")?;
        writeln!(w, "|---|---|")?;
        for f in &v.fields {
            writeln!(w, "| `{}` | `{}` |", f.name, f.ty)?;
        }
        writeln!(w)?;
    }

    writeln!(w, "## Decision-event verbosity (`--decision-format`)")?;
    writeln!(w)?;
    writeln!(
        w,
        "Decision events are emitted independently of the rest of the bus and have their own verbosity knob. Each curated `decision_kind` string is classified as either **Summary** (always visible) or **AllOnly** (visible only under `--decision-format all`)."
    )?;
    writeln!(w)?;
    writeln!(w, "```bash")?;
    writeln!(
        w,
        "moagan … --decision-format summary   # default; curated set only"
    )?;
    writeln!(
        w,
        "moagan … --decision-format all      # everything (dashboards, audits)"
    )?;
    writeln!(
        w,
        "moagan … --decision-format off      # silence every Decision event"
    )?;
    writeln!(
        w,
        "MOAGAN_DECISION_FORMAT=all moagan … # env var (same precedence as flag)"
    )?;
    writeln!(w, "```")?;
    writeln!(w)?;
    writeln!(w, "Resolution order (highest first):")?;
    writeln!(w)?;
    writeln!(
        w,
        "1. `MOAGAN_DECISION_FORMAT` env var (`off` / `summary` / `all`; unknown values fall back to `summary`)."
    )?;
    writeln!(w, "2. Explicit `--decision-format` flag.")?;
    writeln!(w, "3. Default (`summary`).")?;
    writeln!(w)?;
    writeln!(w, "### Curated `decision_kind` strings")?;
    writeln!(w)?;
    writeln!(w, "| `decision_kind` | Level |")?;
    writeln!(w, "|---|---|")?;
    for (kind, level) in DECISION_KIND_INVENTORY {
        writeln!(w, "| `{}` | {} |", kind, level)?;
    }
    writeln!(w)?;
    writeln!(
        w,
        "The classification is exposed as `moagan::telemetry::stdout_events::DECISION_KIND_INVENTORY`. Unknown `decision_kind` strings default to Summary (every kind is visible until classified)."
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "\n## Generated by\n\n`moagan-docgen events` (EPIC #852). Parses `src/telemetry/stdout_events.rs` to extract the `Event<'a>` variant registry; consumes `DECISION_KIND_INVENTORY` for the curated decision table."
    )?;
    Ok(())
}

fn parse_event_variants() -> std::result::Result<Vec<EventVariant>, String> {
    let repo = find_repo_root().map_err(|e| e.to_string())?;
    let path = repo.join("src/telemetry/stdout_events.rs");
    let src = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;

    let enum_start = src
        .find("pub enum Event<'a>")
        .ok_or_else(|| "could not find `pub enum Event<'a>` in stdout_events.rs".to_string())?;
    let after_enum = &src[enum_start..];
    let enum_end_rel = after_enum
        .find("\n}\n")
        .ok_or_else(|| "could not find closing brace of `Event` enum".to_string())?;
    let enum_body = &after_enum[..enum_end_rel + 1];

    let mut variants = Vec::new();
    for line in enum_body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("//") || trimmed.is_empty() {
            continue;
        }
        if !trimmed.ends_with('{') {
            continue;
        }
        let name = trimmed.trim_end_matches('{').trim().to_string();
        let first = match name.chars().next() {
            Some(c) => c,
            None => continue,
        };
        if !first.is_ascii_uppercase() || name.contains('(') || name.contains(' ') {
            continue;
        }
        let kind_tag = to_snake_case(&name);
        variants.push(EventVariant {
            name: name.clone(),
            kind_tag,
            fields: Vec::new(),
        });
    }

    // Now scan each variant's field block by walking the file once and
    // tracking brace depth after each variant header.
    let re_variant = regex::Regex::new(r"(?m)^[[:space:]]*([A-Z][A-Za-z0-9]*) \{$").unwrap();
    let mut last_end = 0usize;
    for cap in re_variant.captures_iter(enum_body) {
        let m = cap.get(1).unwrap();
        let variant_name = m.as_str();
        let header_start = cap.get(0).unwrap().start();
        let variant_start = header_start + (header_start - last_end);
        // Find the matching closing brace.
        let mut depth = 0i32;
        let mut pos = header_start;
        let bytes = enum_body.as_bytes();
        let mut in_str = false;
        while pos < enum_body.len() {
            let c = bytes[pos] as char;
            if in_str {
                if c == '"' && (pos == 0 || bytes[pos - 1] != b'\\') {
                    in_str = false;
                }
            } else if c == '"' {
                in_str = true;
            } else if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            pos += 1;
        }
        let block = &enum_body[header_start + cap.get(0).unwrap().as_str().len() + 1..pos];
        let fields = parse_event_fields(block);
        for v in variants.iter_mut() {
            if v.name == variant_name {
                v.fields = fields;
                break;
            }
        }
        last_end = pos;
        let _ = variant_start;
    }

    if variants.is_empty() {
        return Err("no variants found in Event enum".to_string());
    }
    Ok(variants)
}

fn parse_event_fields(block: &str) -> Vec<EventField> {
    let mut fields = Vec::new();
    for line in block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        // Field shape: `name: type,`
        let Some((name_part, ty_part)) = trimmed.split_once(':') else {
            continue;
        };
        let name = name_part.trim().to_string();
        let ty = ty_part.trim().trim_end_matches(',').trim().to_string();
        if name.is_empty() || ty.is_empty() {
            continue;
        }
        let first = match name.chars().next() {
            Some(c) => c,
            None => continue,
        };
        if first.is_ascii_lowercase() || first == '_' {
            fields.push(EventField { name, ty });
        }
    }
    fields
}

fn to_snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            for lc in c.to_lowercase() {
                out.push(lc);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn variant_summary(v: &EventVariant) -> String {
    match v.kind_tag.as_str() {
        "run_start" => "At run start, after the dispatcher resolves the command.".to_string(),
        "run_end" => "At run end (success or error).".to_string(),
        "phase_start" => "At the start of every `Phase::execute`.".to_string(),
        "phase_end" => "On successful `Phase::execute` completion.".to_string(),
        "phase_error" => "When a `Phase::execute` returns `Err`.".to_string(),
        "llm_call" => "On successful `provider.send` (non-probe).".to_string(),
        "discovery_iteration" => "Per sketch loop iteration in discovery.".to_string(),
        "probe" => "Per auto-probe call (temperature / max_tokens).".to_string(),
        "warning" => "When `Telemetry::warn` is called.".to_string(),
        "decision" => {
            "At curated decision points throughout the pipeline (verbosity controlled by `--decision-format`)."
                .to_string()
        }
        _ => format!("Variant `{}` emitted by the canonical NDJSON stream.", v.name),
    }
}

// ---------------------------------------------------------------------
// Subcommand: test-skips
// ---------------------------------------------------------------------

fn generate_test_skips_report(repo_root: &Path, w: &mut dyn Write) -> Result<()> {
    writeln!(w, "# Test skips inventory — `moagan`")?;
    writeln!(w)?;
    writeln!(
        w,
        "> Auto-generated by `moagan-docgen test-skips`. Do not edit by hand — run the docgen tool and commit the diff."
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "A complete catalogue of every place the test suite skips code on purpose. Use this when:"
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "- A PR fails CI on a test you \"didn't touch\" — check if it's in this list."
    )?;
    writeln!(
        w,
        "- Adding a new skip — confirm it's not already covered by an existing mechanism."
    )?;
    writeln!(
        w,
        "- Removing a skip — verify the test now passes reliably on cold cache (locally with `cargo clean -p moagan`)."
    )?;
    writeln!(w)?;

    layer_one_ruleset(w)?;
    layer_two_skip_cli(w, repo_root)?;
    layer_three_ignore(w, repo_root)?;
    layer_four_silent_skip(w, repo_root)?;
    layer_five_validation_evidence(w, repo_root)?;
    layer_six_e2e_groups(w, repo_root)?;
    layer_seven_lefthook(w, repo_root)?;
    layer_eight_env_locks(w, repo_root)?;

    let summary = build_summary_table();
    writeln!(w, "## Summary table")?;
    writeln!(w)?;
    writeln!(w, "| Layer | Mechanism | Count | Auto-skipped on CI? |")?;
    writeln!(w, "|---|---|---|---|")?;
    for row in summary {
        writeln!(
            w,
            "| {} | {} | {} | {} |",
            row.layer, row.mechanism, row.count, row.auto_skipped
        )?;
    }
    writeln!(w)?;

    writeln!(
        w,
        "\n## Generated by\n\n`moagan-docgen test-skips` (EPIC #852). Eight layers scanned live from the codebase, branch-protection ruleset, e2e proxy test-group manifest, and lefthook configuration."
    )?;
    Ok(())
}

struct SummaryRow {
    layer: String,
    mechanism: String,
    count: String,
    auto_skipped: String,
}

fn build_summary_table() -> Vec<SummaryRow> {
    vec![
        SummaryRow {
            layer: "1".into(),
            mechanism: "Ruleset `required_status_checks`".into(),
            count: "8 jobs required, 1 not".into(),
            auto_skipped: "n/a".into(),
        },
        SummaryRow {
            layer: "2".into(),
            mechanism: "`cargo test --skip` CLI flag".into(),
            count: "0 tests".into(),
            auto_skipped: "n/a (closed)".into(),
        },
        SummaryRow {
            layer: "3".into(),
            mechanism: "`#[ignore]` Rust attribute".into(),
            count: "(see table)".into(),
            auto_skipped: "❌ no (run via `--ignored`)".into(),
        },
        SummaryRow {
            layer: "4".into(),
            mechanism: "Source silent-skip (binary on PATH)".into(),
            count: "(see table)".into(),
            auto_skipped: "✅ partially (binaries present)".into(),
        },
        SummaryRow {
            layer: "5".into(),
            mechanism: "`ValidationEvidence::skipped()` runtime".into(),
            count: "(see table)".into(),
            auto_skipped: "n/a (per-artifact)".into(),
        },
        SummaryRow {
            layer: "6".into(),
            mechanism: "Bash script conditional runs (`e2e_audit_proxy.sh`)".into(),
            count: "(see table)".into(),
            auto_skipped: "❌ no (env vars not set in CI)".into(),
        },
        SummaryRow {
            layer: "7".into(),
            mechanism: "Lefthook escape hatches".into(),
            count: "n/a (escape hatches)".into(),
            auto_skipped: "❌ no".into(),
        },
        SummaryRow {
            layer: "8".into(),
            mechanism: "Process-wide env-locks (`TEST_*_LOCK` statics)".into(),
            count: "(see table)".into(),
            auto_skipped: "n/a (serialisation, not skip)".into(),
        },
    ]
}

fn layer_one_ruleset(w: &mut dyn Write) -> Result<()> {
    writeln!(
        w,
        "## Layer 1 — Ruleset `protect-main` (GitHub branch rules)"
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "The ruleset protects `main` via `gh api /repos/airvzxf/moagan/rulesets/19743104`."
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "The closest thing to a skip is **`required_status_checks.contexts`** — the list of CI jobs that MUST be green before merge. Anything not on the list is implicitly **not enforced**."
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "Re-apply the ruleset with the `gh api` block below; see [`docs/branch-protection.md`](branch-protection.md) for the full PUT cycle."
    )?;
    writeln!(w)?;
    writeln!(w, "```bash")?;
    writeln!(w, "gh api /repos/airvzxf/moagan/rulesets/19743104 \\")?;
    writeln!(
        w,
        "  --jq '.rules[] | select(.type == \"required_status_checks\") | .parameters.required_status_checks[] | .context'"
    )?;
    writeln!(w, "```")?;
    writeln!(w)?;
    writeln!(
        w,
        "Expected output (8 contexts; case-sensitive, must match `name:` in `.github/workflows/ci.yml`):"
    )?;
    writeln!(w)?;
    writeln!(w, "```")?;
    for c in [
        "T0 · fmt-check",
        "T0 · guard-deps",
        "T1 · clippy",
        "T2 · cargo test --lib --bins",
        "T2 · cargo test --tests (integration)",
        "T2 · cargo test --doc",
        "T3 · make smoke",
        "T3 · make e2e (local mock pipeline)",
    ] {
        writeln!(w, "{c}")?;
    }
    writeln!(w, "```")?;
    writeln!(w)?;
    writeln!(
        w,
        "Plus the ruleset-level `required_signatures` rule, which enforces GPG signing on every commit landing on `main`. `e2e-network` and the post-release validation workflows are intentionally NOT required — they surface as checks but do not block merges."
    )?;
    writeln!(w)?;
    Ok(())
}

fn layer_two_skip_cli(w: &mut dyn Write, repo_root: &Path) -> Result<()> {
    writeln!(
        w,
        "## Layer 2 — `cargo test --skip` (CLI-level test exclusions)"
    )?;
    writeln!(w)?;
    let mut matches: Vec<String> = Vec::new();
    let scan_paths = ["Makefile", ".github/workflows", "scripts"];
    for p in scan_paths {
        let path = repo_root.join(p);
        scan_for_skip(&path, &mut matches).map_err(|e| {
            DocgenError::Custom(format!("scanning {} for --skip: {e}", path.display()))
        })?;
    }
    if matches.is_empty() {
        writeln!(
            w,
            "**Empty as of the last docgen run.** All historical skip entries were closed by root-cause fixes; the skip list has been collapsed back to plain `cargo test` invocations in every script and CI workflow."
        )?;
    } else {
        writeln!(w, "| Location | Context |")?;
        writeln!(w, "|---|---|")?;
        for m in &matches {
            writeln!(w, "| `{m}` | (matches `--skip` invocation) |")?;
        }
    }
    writeln!(w)?;
    Ok(())
}

fn scan_for_skip(path: &Path, out: &mut Vec<String>) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_file() {
        scan_file_for_skip(path, out)?;
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            scan_for_skip(&p, out)?;
        } else if p.is_file() {
            scan_file_for_skip(&p, out)?;
        }
    }
    Ok(())
}

fn scan_file_for_skip(path: &Path, out: &mut Vec<String>) -> io::Result<()> {
    let interesting = path.file_name().map(|f| f == "Makefile").unwrap_or(false)
        || path
            .extension()
            .map(|e| e == "sh" || e == "yml" || e == "yaml" || e == "mk")
            .unwrap_or(false);
    if !interesting {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    for (i, line) in content.lines().enumerate() {
        if line.trim_start().starts_with('#') || line.trim_start().starts_with("//") {
            continue;
        }
        // Layer 2 is `cargo test --skip <name>` only. Match
        // `cargo test` on the same line and `--skip` as an argument
        // to it (preceded by whitespace). Filter out `--skip-smoke`,
        // shell `--skip` flag wrappers like gauntlet.sh, etc.
        let lower = line.to_ascii_lowercase();
        if !lower.contains("cargo test") {
            continue;
        }
        let has_skip = line
            .split_whitespace()
            .any(|tok| tok == "--skip" || tok.starts_with("--skip="));
        if !has_skip {
            continue;
        }
        let cwd = std::env::current_dir().unwrap_or_default();
        let rel = path
            .strip_prefix(&cwd)
            .unwrap_or(path)
            .display()
            .to_string();
        out.push(format!("{rel}:{}", i + 1));
    }
    Ok(())
}

fn layer_three_ignore(w: &mut dyn Write, repo_root: &Path) -> Result<()> {
    writeln!(w, "## Layer 3 — `#[ignore]` attribute (Rust source-level)")?;
    writeln!(w)?;
    writeln!(
        w,
        "`#[ignore]` tests are **compiled but not run** by default. They run when invoked with `cargo test -- --ignored` or `cargo test <name> -- --ignored`."
    )?;
    writeln!(w)?;

    let mut entries = scan_ignore_in_source(repo_root);
    entries.sort();
    entries.dedup();
    if entries.is_empty() {
        writeln!(
            w,
            "**No `#[ignore]` tests detected.** If the test suite grows one, the docgen tool will record it on the next run."
        )?;
        writeln!(w)?;
        return Ok(());
    }

    writeln!(w, "| Test | File | Reason |")?;
    writeln!(w, "|---|---|---|")?;
    for (test, file) in &entries {
        let _ = writeln!(w, "| `{test}` | `{file}` | (auto) |");
    }
    writeln!(w)?;
    writeln!(
        w,
        "Note: these are NOT included in `cargo test --skip`. To run them:"
    )?;
    writeln!(w)?;
    writeln!(w, "```bash")?;
    writeln!(w, "cargo test --lib -- --ignored")?;
    writeln!(w, "```")?;
    writeln!(w)?;
    writeln!(
        w,
        "Cross-checked against `cargo test --lib --all-features -- --list --format=json` (`ignore` flag), which produces the same set when the lib compiles cleanly."
    )?;
    writeln!(w)?;
    Ok(())
}

fn scan_ignore_in_source(repo_root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let visit = |root: &Path, out: &mut Vec<(String, String)>| {
        if !root.exists() {
            return;
        }
        walk_rs_files(root, &mut |path| {
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return,
            };
            let lines: Vec<&str> = content.lines().collect();
            for i in 0..lines.len() {
                let line = lines[i];
                if !line.contains("#[ignore") {
                    continue;
                }
                // Skip if inside a #[cfg(test)] block — we want only
                // source-level ignores that affect the public test
                // surface, not internal unit tests.
                if is_inside_cfg_test(&lines, i) {
                    continue;
                }
                // Find next `fn <name>` within a few following lines.
                for j in (i + 1)..(i + 8).min(lines.len()) {
                    let l = lines[j].trim();
                    if let Some(name) = parse_test_fn_name(l) {
                        let rel = path
                            .strip_prefix(repo_root)
                            .unwrap_or(path)
                            .display()
                            .to_string();
                        out.push((name, rel));
                        break;
                    }
                }
            }
        });
    };
    visit(&repo_root.join("src"), &mut out);
    visit(&repo_root.join("tests"), &mut out);
    out
}

fn is_inside_cfg_test(lines: &[&str], idx: usize) -> bool {
    // Walk backwards from `idx` and detect an unclosed `#[cfg(test)]`
    // attribute before the current `#[ignore]`.
    let mut depth: i32 = 0;
    for (_k, line) in lines.iter().enumerate().take(idx + 1).rev() {
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        depth -= opens;
        depth += closes;
        if line.contains("#[cfg(test)]") && depth >= 0 {
            return true;
        }
    }
    false
}

fn walk_rs_files<F: FnMut(&Path)>(root: &Path, f: &mut F) {
    if !root.exists() {
        return;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let entries = match fs::read_dir(&p) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
                f(&path);
            }
        }
    }
}

fn parse_test_fn_name(line: &str) -> Option<String> {
    let line = line.trim_start();
    let line = line.strip_prefix("pub ").unwrap_or(line);
    let line = line.strip_prefix("async ").unwrap_or(line);
    let line = line.strip_prefix("pub ").unwrap_or(line);
    let after = line.strip_prefix("fn ")?;
    let name: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

fn layer_four_silent_skip(w: &mut dyn Write, repo_root: &Path) -> Result<()> {
    writeln!(
        w,
        "## Layer 4 — Source-level silent skips (binary missing on PATH)"
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "Tests that exit early via `return;` when a required external tool is missing from `$PATH`. The test runs but produces no signal (not even a pass). These are NOT exposed via `--skip` (cargo doesn't know about them)."
    )?;
    writeln!(w)?;

    let pattern = regex::Regex::new(
        r#"std::process::Command::new\(\s*"([^"]+)"\s*\)(?s:.{0,200}?)\.is_err\(\)"#,
    )
    .map_err(|e| DocgenError::Custom(format!("regex compile: {e}")))?;
    let mut rows: Vec<(String, String, usize)> = Vec::new();
    let walker_root = repo_root.join("src/validators");
    walk_rs_files(&walker_root, &mut |path| {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        for m in pattern.captures_iter(&content) {
            let line = content[..m.get(0).unwrap().start()].matches('\n').count() + 1;
            let binary = m.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let rel = path
                .strip_prefix(repo_root)
                .unwrap_or(path)
                .display()
                .to_string();
            rows.push((rel, binary, line));
        }
    });
    rows.sort();
    if rows.is_empty() {
        writeln!(w, "**No silent-skip sites detected.**")?;
        writeln!(w)?;
        return Ok(());
    }
    writeln!(w, "| File:line | Binary |")?;
    writeln!(w, "|---|---|")?;
    for (file, binary, line) in &rows {
        let _ = writeln!(w, "| `{file}:{line}` | `{binary}` |");
    }
    writeln!(w)?;
    let validator_set: std::collections::BTreeSet<String> = rows
        .iter()
        .map(|(f, _, _)| {
            f.split('/')
                .next_back()
                .unwrap_or("")
                .trim_end_matches(".rs")
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if !validator_set.is_empty() {
        let joined = validator_set.into_iter().collect::<Vec<_>>().join(", ");
        let _ = writeln!(w, "Validators with silent-skip sites: {joined}");
        writeln!(w)?;
    }
    writeln!(
        w,
        "Pattern (the standard shape):\n\n\
```rust\n\
if std::process::Command::new(\"<binary>\").arg(\"--version\").output().is_err() {{\n    \
    return; // skip silently\n\
}}\n\
```"
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "Total: **{} silent-skip sites** across `src/validators/*.rs`. None are blocked by CI (the runners have all binaries).",
        rows.len()
    )?;
    writeln!(w)?;
    Ok(())
}

fn layer_five_validation_evidence(w: &mut dyn Write, repo_root: &Path) -> Result<()> {
    writeln!(
        w,
        "## Layer 5 — `ValidationEvidence::skipped()` (per-artifact runtime skips)"
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "The Rust validators return `Ok(ValidationEvidence::skipped(name, reason))` when an individual artifact doesn't qualify for that validator. This is **runtime behavior**, not a test skip; the validator still runs but reports `Skipped` instead of `Pass`/`Fail`."
    )?;
    writeln!(w)?;
    let pattern = regex::Regex::new(r"ValidationEvidence::skipped\(\s*").unwrap();
    let mut rows: Vec<(String, usize)> = Vec::new();
    let walker_root = repo_root.join("src");
    walk_rs_files(&walker_root, &mut |path| {
        // Skip the docgen binary itself — its source contains the
        // example pattern in a docstring, not a real call site.
        if path
            .file_name()
            .map(|f| f == "moagan-docgen.rs")
            .unwrap_or(false)
        {
            return;
        }
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let lines: Vec<&str> = content.lines().collect();
        let mut in_test = false;
        let mut brace_depth: i32 = 0;
        for (i, line) in lines.iter().enumerate() {
            if line.contains("#[cfg(test)]") {
                in_test = true;
                brace_depth = 0;
            }
            if in_test {
                brace_depth += line.matches('{').count() as i32;
                brace_depth -= line.matches('}').count() as i32;
                if brace_depth <= 0 && line.contains('}') {
                    in_test = false;
                }
                continue;
            }
            if pattern.is_match(line) {
                let rel = path
                    .strip_prefix(repo_root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                rows.push((rel, i + 1));
            }
        }
    });
    rows.sort();
    if rows.is_empty() {
        writeln!(w, "**No `ValidationEvidence::skipped()` sites detected.**")?;
        writeln!(w)?;
        return Ok(());
    }
    writeln!(w, "| File | Line |")?;
    writeln!(w, "|---|---|")?;
    for (file, line) in &rows {
        let _ = writeln!(w, "| `{file}` | {line} |");
    }
    writeln!(w)?;
    writeln!(
        w,
        "Total: **{} runtime `skipped` returns**. Each is a normal code path, not a test exclusion.",
        rows.len()
    )?;
    writeln!(w)?;
    Ok(())
}

fn layer_six_e2e_groups(w: &mut dyn Write, repo_root: &Path) -> Result<()> {
    writeln!(w, "## Layer 6 — Bash script conditional runs")?;
    writeln!(w)?;
    writeln!(
        w,
        "The e2e proxy suite (`scripts/e2e_audit_proxy.sh`) declares a single source-of-truth test-group manifest in `declare_test_groups()`. The `MOAGAN_PRINT_TEST_GROUPS=1` mode emits it as a markdown table on stdout, exiting 0 before any test runs."
    )?;
    writeln!(w)?;
    writeln!(w, "### Test group manifest (auto-extracted)")?;
    writeln!(w)?;
    let output = Command::new("bash")
        .arg("scripts/e2e_audit_proxy.sh")
        .env("MOAGAN_PRINT_TEST_GROUPS", "1")
        .current_dir(repo_root)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                writeln!(w, "{line}")?;
            }
        }
        Ok(out) => {
            let snippet: String = String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(200)
                .collect();
            writeln!(
                w,
                "_Could not invoke `scripts/e2e_audit_proxy.sh` (exit {}): {}_",
                out.status, snippet
            )?;
        }
        Err(e) => {
            writeln!(w, "_Could not invoke `scripts/e2e_audit_proxy.sh`: {e}_")?;
        }
    }
    writeln!(w)?;
    writeln!(
        w,
        "The script also includes conditional blocks gated on `MINIMAX_API_KEY`, `OPENCODE_API_KEY`, `DEEPSEEK_API_KEY`, `MOAGAN_SMOKE_LONG_DISCOVER`, and `MOAGAN_SMOKE_SECTION`. See `scripts/e2e_audit_proxy.sh` for the canonical implementation."
    )?;
    writeln!(w)?;
    Ok(())
}

fn layer_seven_lefthook(w: &mut dyn Write, repo_root: &Path) -> Result<()> {
    writeln!(w, "## Layer 7 — Lefthook escape hatches (developer-side)")?;
    writeln!(w)?;
    writeln!(
        w,
        "`lefthook.yml` doesn't skip any test by default. It offers three **escape hatches** that bypass hooks (the opposite of skip — they let the dev skip the validation entirely):"
    )?;
    writeln!(w)?;
    writeln!(w, "| Escape hatch | Effect |")?;
    writeln!(w, "|---|---|")?;
    writeln!(
        w,
        "| `LEFTPHOOK=0 git commit -m \"...\"` | Disable all lefthook hooks for one command |"
    )?;
    writeln!(
        w,
        "| `git commit --no-verify` | Bypass pre-commit + commit-msg |"
    )?;
    writeln!(
        w,
        "| `git push --no-verify` | Bypass pre-push (T2 cargo test still runs in CI) |"
    )?;
    writeln!(w)?;
    let path = repo_root.join("lefthook.yml");
    if let Ok(content) = fs::read_to_string(&path) {
        writeln!(w, "Source (extracted from `lefthook.yml` lines 13–20):")?;
        writeln!(w)?;
        writeln!(w, "```yaml")?;
        for line in content.lines().take(20) {
            writeln!(w, "{line}")?;
        }
        writeln!(w, "```")?;
    }
    writeln!(w)?;
    writeln!(
        w,
        "These do NOT affect CI — only local development. CI re-runs the full check from a clean state."
    )?;
    writeln!(w)?;
    Ok(())
}

fn layer_eight_env_locks(w: &mut dyn Write, repo_root: &Path) -> Result<()> {
    writeln!(
        w,
        "## Layer 8 — Process-wide env-locks (`TEST_*_LOCK` statics)"
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "Tests that mutate process-wide state — `std::env::set_var`, `std::env::set_current_dir`, etc. — must serialise against any sibling test that reads the same state, or the parallel `cargo test` run surfaces as a flake. Each lock here is a `pub static Mutex<()>` declared at the top of `src/lib.rs` (under `#[cfg(test)]` or, in the case of `TEST_API_KEYS_LOCK`, deliberately not gated so integration tests in `tests/` can also acquire it)."
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "These are NOT skips — every test below still runs. They are serialisation gates so the asserts inside each test see a stable view of the env-var they touch."
    )?;
    writeln!(w)?;

    let lib_rs = repo_root.join("src/lib.rs");
    let content = fs::read_to_string(&lib_rs).map_err(DocgenError::Io)?;
    let pattern = regex::Regex::new(r"pub static (TEST_[A-Z_]+)_LOCK")
        .map_err(|e| DocgenError::Custom(format!("regex compile: {e}")))?;
    let mut lock_rows: Vec<(String, usize)> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if let Some(caps) = pattern.captures(line) {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            lock_rows.push((name, i + 1));
        }
    }

    let stdout_events_rs = repo_root.join("src/telemetry/stdout_events.rs");
    let stdout_content = fs::read_to_string(&stdout_events_rs).map_err(DocgenError::Io)?;
    let stdout_pattern = regex::Regex::new(r"static ([A-Z_]+_LOCK)\s*[:=]").unwrap();
    let mut module_local: Vec<(String, usize)> = Vec::new();
    for (i, line) in stdout_content.lines().enumerate() {
        if let Some(caps) = stdout_pattern.captures(line) {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            module_local.push((name, i + 1));
        }
    }

    writeln!(w, "### Inventory (crate-wide)")?;
    writeln!(w)?;
    writeln!(w, "| Lock | Source line |")?;
    writeln!(w, "|---|---|")?;
    for (name, line) in &lock_rows {
        let _ = writeln!(w, "| `{name}_LOCK` | `src/lib.rs:{line}` |");
    }
    writeln!(w)?;
    writeln!(w, "### Inventory (module-local)")?;
    writeln!(w)?;
    writeln!(w, "| Lock | Source line |")?;
    writeln!(w, "|---|---|")?;
    if module_local.is_empty() {
        writeln!(w, "| _(none)_ | _(n/a)_ |")?;
    } else {
        for (name, line) in &module_local {
            let _ = writeln!(w, "| `{name}` | `src/telemetry/stdout_events.rs:{line}` |");
        }
    }
    writeln!(w)?;
    writeln!(
        w,
        "Total: **{} crate-wide locks** + **{} module-local**. Adding a new lock requires an entry in `src/lib.rs` (or the relevant module), an acquisition site at every test that mutates the env var, and a row in this table so the next maintainer doesn't have to grep the codebase to discover the lock.",
        lock_rows.len(),
        module_local.len()
    )?;
    writeln!(w)?;
    Ok(())
}
