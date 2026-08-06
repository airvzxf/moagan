//! D.17.9: tracing filter helper.

/// Return the recommended `EnvFilter`-style directive. Honors
/// `RUST_LOG` when set; otherwise returns a sane baseline that
/// turns the library up to `debug` and the telemetry layer to `trace`.
pub fn recommended_env_filter() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| "info,moagan=debug,moagan::telemetry=trace".into())
}
