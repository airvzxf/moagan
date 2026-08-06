//! D.17.8: bundled dashboard HTML.

/// The dashboard markup. Self-contained (`<style>` + `<script>` inline,
/// no external dependencies); consumers can copy or embed verbatim.
#[allow(missing_docs)]
pub const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html><head><title>moagan dashboard</title>
<style>body{font-family:monospace;padding:1rem}table{border-collapse:collapse}
td,th{border:1px solid #ccc;padding:.25rem .5rem}</style></head>
<body><h1>moagan dashboard</h1>
<div id="runs">Loading...</div>
<script>
async function load() {
  const r = await fetch('/api/runs');
  const j = await r.json();
  const rows = j.runs.map(run => `<tr>
    <td>${run.run_id}</td><td>${run.mode}</td><td>${run.status}</td>
    <td>${run.tokens}</td></tr>`).join('');
  document.getElementById('runs').innerHTML =
    `<table><thead><tr><th>run_id</th><th>mode</th><th>status</th><th>tokens</th></tr></thead>
    <tbody>${rows}</tbody></table>`;
}
load();
</script></body></html>"#;

/// Write the dashboard HTML to `<out_dir>/dashboard.html`.
pub fn write_dashboard(out_dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    std::fs::write(out_dir.join("dashboard.html"), DASHBOARD_HTML)
}
