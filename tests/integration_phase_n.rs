use moagan::sandbox::{
    COMMAND_CONFIGS, Sandbox, SandboxConfig, SandboxError, SandboxStatus, config_for,
    strip_secrets, verify_binary_exists,
};

#[tokio::test]
async fn sandbox_caps_stdout_at_max_bytes() {
    let sandbox = Sandbox::new(SandboxConfig::new().with_max_capture(128)).unwrap();
    let payload = "x".repeat(4096);
    let result = sandbox.run("echo", &[payload.as_str()]).await;
    assert!(matches!(result, Err(SandboxError::OutputTruncated)));
}

#[tokio::test]
async fn strip_secrets_applies_to_real_args() {
    let sandbox = Sandbox::new(SandboxConfig::new()).unwrap();
    let secret = "sk-cp-abcdefghijklmnopqrstuvwxyz";
    let result = sandbox
        .run("sh", &["-c", "printf '%s' \"$1\"", "sandbox", secret])
        .await
        .unwrap();
    assert_eq!(result.status, SandboxStatus::Pass);
    assert!(!result.stdout.contains(secret));
    assert!(result.stdout.contains("REDACTED"));
    assert!(!result.command.contains(secret));
}

#[test]
fn verify_binary_exists_passes_for_rustc() {
    assert!(verify_binary_exists("rustc").is_ok());
}

#[test]
fn verify_binary_exists_fails_for_missing_binary() {
    let result = verify_binary_exists("moagan-phase-n-missing-binary");
    assert!(matches!(
        result,
        Err(SandboxError::BinaryNotFound(binary))
            if binary == "moagan-phase-n-missing-binary"
    ));
}

#[tokio::test]
async fn sandbox_caps_stderr_at_max_bytes() {
    let sandbox = Sandbox::new(SandboxConfig::new().with_max_capture(128)).unwrap();
    let payload = "y".repeat(4096);
    let result = sandbox
        .run(
            "sh",
            &["-c", r#"printf '%s' "$1" >&2"#, "sandbox", payload.as_str()],
        )
        .await;
    assert!(matches!(result, Err(SandboxError::OutputTruncated)));
}

#[test]
fn strip_secrets_keeps_real_argument_layout() {
    let args = vec![
        "--api-key".to_owned(),
        "sk-cp-abcdefghijklmnopqrstuvwxyz".to_owned(),
        "--mode".to_owned(),
        "fast".to_owned(),
    ];
    let stripped = strip_secrets(&args);
    assert_eq!(stripped.len(), args.len());
    assert_eq!(stripped[0], args[0]);
    assert_eq!(stripped[2..], args[2..]);
    assert!(!stripped[1].contains("sk-cp-"));
}

#[test]
fn command_configs_cover_all_supported_languages() {
    assert_eq!(COMMAND_CONFIGS.len(), 4);
    for name in ["rust", "python", "typescript", "sql"] {
        assert!(config_for(name).is_some());
    }
}
