//! Regex patterns for secret redaction. Compiled once, reused.
//!
//! Compliance: T01-06 §5.2 (22 patterns) + 10-integrada-v0 §D.8 (12 more).
//! All patterns are case-insensitive unless the protocol is well-known
//! to be case-sensitive (e.g. JWT base64).

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// A named pattern with a human-readable replacement marker.
pub struct Pattern {
    /// Stable identifier for the pattern (e.g. `"openai_key"`).
    pub id: &'static str,
    /// Compiled regex.
    pub re: Regex,
    /// Replacement marker. The original is replaced with this exact
    /// string (e.g. `"[REDACTED:openai_key]"`).
    pub replacement: &'static str,
}

macro_rules! pat {
    ($id:literal, $re:expr, $rep:expr) => {
        Pattern {
            id: $id,
            re: Regex::new($re).expect(concat!("invalid regex in pattern ", $id)),
            replacement: $rep,
        }
    };
}

/// Built-in pattern library. Order matters: more specific patterns first
/// to avoid the generic "bearer" masking an "anthropic-key".
pub fn builtin_patterns() -> Vec<Pattern> {
    vec![
        pat!(
            "minimax_sk_cp",
            r"sk-cp-[A-Za-z0-9_\-]{16,}",
            "[REDACTED:minimax_sk_cp]"
        ),
        pat!(
            "anthropic_key",
            r"sk-ant-[A-Za-z0-9_-]{20,}",
            "[REDACTED:anthropic_key]"
        ),
        pat!(
            "openai_key",
            r"sk-[A-Za-z0-9]{20,}",
            "[REDACTED:openai_key]"
        ),
        pat!(
            "gemini_key",
            r"AIzaSy[A-Za-z0-9_-]{20,}",
            "[REDACTED:gemini_key]"
        ),
        pat!(
            "huggingface_token",
            r"hf_[A-Za-z0-9]{20,}",
            "[REDACTED:huggingface_token]"
        ),
        pat!(
            "replicate_token",
            r"r8_[A-Za-z0-9]{20,}",
            "[REDACTED:replicate_token]"
        ),
        pat!(
            "elevenlabs_key",
            r"\b[a-f0-9]{32}\b",
            "[REDACTED:elevenlabs_key]"
        ),
        pat!(
            "github_pat",
            r"ghp_[A-Za-z0-9]{36}",
            "[REDACTED:github_pat]"
        ),
        pat!(
            "github_oauth",
            r"gho_[A-Za-z0-9]{36}",
            "[REDACTED:github_oauth]"
        ),
        pat!(
            "github_app",
            r"(ghu|ghs)_[A-Za-z0-9]{36}",
            "[REDACTED:github_app]"
        ),
        pat!(
            "aws_access_key",
            r"AKIA[0-9A-Z]{16}",
            "[REDACTED:aws_access_key]"
        ),
        pat!(
            "aws_secret_key",
            r#"(?i)aws_secret_access_key\s*=\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#,
            "[REDACTED:aws_secret_key]"
        ),
        pat!(
            "bearer",
            r"(?i)bearer\s+[A-Za-z0-9_\-\.=]{16,}",
            "[REDACTED:bearer]"
        ),
        pat!(
            "jwt",
            r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
            "[REDACTED:jwt]"
        ),
        pat!(
            "ssh_private_key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]+?-----END [A-Z ]*PRIVATE KEY-----",
            "[REDACTED:ssh_private_key]"
        ),
        pat!(
            "pem_certificate",
            r"-----BEGIN CERTIFICATE-----[\s\S]+?-----END CERTIFICATE-----",
            "[REDACTED:pem_certificate]"
        ),
        pat!(
            "connection_string",
            r#"(?i)(postgres|postgresql|mysql|mongodb|redis|amqp)://[^\s"']{8,}"#,
            "[REDACTED:connection_string]"
        ),
        pat!(
            "slack_token",
            r"xox[abprs]-[A-Za-z0-9-]{10,}",
            "[REDACTED:slack_token]"
        ),
        pat!(
            "stripe_live",
            r"sk_live_[A-Za-z0-9]{24,}",
            "[REDACTED:stripe_live]"
        ),
        pat!(
            "stripe_test",
            r"sk_test_[A-Za-z0-9]{24,}",
            "[REDACTED:stripe_test]"
        ),
        pat!(
            "sendgrid",
            r"SG\.[A-Za-z0-9_\-]{22}\.[A-Za-z0-9_\-]{43}",
            "[REDACTED:sendgrid]"
        ),
        pat!("twilio", r"SK[0-9a-fA-F]{32}", "[REDACTED:twilio]"),
        pat!(
            "google_oauth_refresh",
            r"1//[0-9A-Za-z_\-]{43,}",
            "[REDACTED:google_oauth_refresh]"
        ),
        pat!(
            "private_ip",
            r"\b(?:(?:10|127)\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3})\b",
            "[REDACTED:private_ip]"
        ),
        pat!(
            "ip_v4",
            r"\b(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\b",
            "[REDACTED:ip_v4]"
        ),
        pat!("ssn_like", r"\b\d{3}-\d{2}-\d{4}\b", "[REDACTED:ssn]"),
        pat!(
            "credit_card",
            r"\b(?:\d[ -]?){13,16}\b",
            "[REDACTED:credit_card]"
        ),
        pat!("email", r"[\w.+-]+@[\w-]+\.[\w.-]+", "[REDACTED:email]"),
        pat!(
            "moagan_minimax_endpoint",
            r"https://api\.minimax\.io/[A-Za-z0-9/_\.\-]+",
            "[REDACTED:moagan_endpoint]"
        ),
    ]
}

/// All built-in patterns, lazily compiled once.
pub static PATTERNS: Lazy<Vec<Pattern>> = Lazy::new(builtin_patterns);

// -----------------------------------------------------------------
// Categorised redaction (proposal-03 §D.8.2)
//
// The categorised substitute replaces the legacy `[REDACTED:id]`
// marker with a shorter `***REDACTED:slug***` shape that makes it
// easier for an operator to grep the redaction log without
// needing the full pattern id. The enum mirrors the 14 kinds
// from the spec; `Unknown` is the catch-all the categorised
// apply pass falls back to when none of the named regexes
// matched.
// -----------------------------------------------------------------

/// Categorised patterns the categorised redaction helper
/// (D.8.2) substitutes in place of the original token. The
/// serialisation is `snake_case` so the JSON sidecars stay
/// human-friendly; the underlying SQLite audit row stores the
/// same string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    /// MiniMax / OpenRouter `sk-cp-...` API key.
    SkCpApiKey,
    /// GitHub personal access token (`ghp_...`, `gho_...`,
    /// `ghu_...`, `ghs_...`).
    GithubPat,
    /// AWS access key id (`AKIA...`).
    AwsAccessKey,
    /// JWT (`eyJ...`).
    Jwt,
    /// `Authorization: Bearer <token>` header.
    BearerHeader,
    /// `password=` / `passwd=` / `pwd=` key-value pair.
    PasswordKv,
    /// RFC-5322-ish email address.
    Email,
    /// Private IPv4 / IPv6 literal.
    PrivateIp,
    /// Credit-card-shaped number (Luhn-free heuristic).
    CreditCard,
    /// PEM-encoded private key block.
    PrivateKey,
    /// Postgres / MySQL / Mongo / Redis / AMQP connection string.
    ConnString,
    /// Anthropic API key (`sk-ant-...`).
    AnthropicApiKey,
    /// OpenAI API key (`sk-...` 20+).
    OpenaiApiKey,
    /// Gemini API key (`AIza...`).
    GeminiApiKey,
    /// Catch-all for matches that did not fit any named kind.
    Unknown,
}

/// Return the categorised substitute string for `kind`. The
/// shape mirrors the spec table: `***REDACTED:<slug>***` for
/// values, `Bearer ***REDACTED***` for the `Authorization`
/// header so the replacement reads as a complete header value.
///
/// Compliance: proposal-03 §D.8.2 (T20-01; T13-09).
pub fn substitute(kind: PatternKind) -> &'static str {
    match kind {
        PatternKind::SkCpApiKey => "***REDACTED:api_key:sk-cp***",
        PatternKind::GithubPat => "***REDACTED:github_pat***",
        PatternKind::AwsAccessKey => "***REDACTED:aws_access_key***",
        PatternKind::Jwt => "***REDACTED:jwt***",
        PatternKind::BearerHeader => "Bearer ***REDACTED***",
        PatternKind::PasswordKv => "***REDACTED:password***",
        PatternKind::Email => "***REDACTED:email***",
        PatternKind::PrivateIp => "***REDACTED:ip***",
        PatternKind::CreditCard => "***REDACTED:cc***",
        PatternKind::PrivateKey => "***REDACTED:private_key***",
        PatternKind::ConnString => "***REDACTED:connstring***",
        PatternKind::AnthropicApiKey => "***REDACTED:api_key:sk-ant***",
        PatternKind::OpenaiApiKey => "***REDACTED:api_key:sk***",
        PatternKind::GeminiApiKey => "***REDACTED:api_key:AIza***",
        PatternKind::Unknown => "***REDACTED***",
    }
}

/// Map a built-in `Pattern` id to its categorised `PatternKind`.
/// Returns `None` for ids that are not part of the categorised
/// catalog (the categorised apply pass skips them silently).
///
/// The mapping is intentionally explicit — every entry maps a
/// legacy `[REDACTED:id]` marker to one of the 14 kinds above.
pub fn kind_for_pattern_id(id: &str) -> Option<PatternKind> {
    match id {
        "minimax_sk_cp" => Some(PatternKind::SkCpApiKey),
        "openai_key" => Some(PatternKind::OpenaiApiKey),
        "anthropic_key" => Some(PatternKind::AnthropicApiKey),
        "gemini_key" => Some(PatternKind::GeminiApiKey),
        "github_pat" | "github_oauth" | "github_app" => Some(PatternKind::GithubPat),
        "aws_access_key" => Some(PatternKind::AwsAccessKey),
        "bearer" => Some(PatternKind::BearerHeader),
        "jwt" => Some(PatternKind::Jwt),
        "pem_private_key" => Some(PatternKind::PrivateKey),
        "ip_v4" => Some(PatternKind::PrivateIp),
        "credit_card" => Some(PatternKind::CreditCard),
        "email" => Some(PatternKind::Email),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_all_patterns() {
        // Force lazy init.
        let _ = &*PATTERNS;
    }

    fn assert_pattern(id: &str, matched: &str, benign: &str) {
        let pattern = PATTERNS.iter().find(|pattern| pattern.id == id);
        let pattern = pattern.unwrap_or_else(|| panic!("missing pattern {id}"));
        assert!(pattern.re.is_match(matched), "{id} did not match sample");
        assert!(!pattern.re.is_match(benign), "{id} matched benign sample");
    }

    #[test]
    fn anthropic_key_matches_only_key_shape() {
        assert_pattern(
            "anthropic_key",
            "key=sk-ant-abcdefghijklmnopqrst",
            "key=anthropic-token",
        );
    }

    #[test]
    fn openai_key_matches_only_key_shape() {
        assert_pattern(
            "openai_key",
            "key=sk-abcdefghijklmnopqrst",
            "key=openai-token",
        );
    }

    #[test]
    fn gemini_key_matches_only_key_shape() {
        assert_pattern(
            "gemini_key",
            "key=AIzaSyabcdefghijklmnopqrst",
            "key=AIza-not-a-key",
        );
    }

    #[test]
    fn huggingface_token_matches_only_token_shape() {
        assert_pattern(
            "huggingface_token",
            "token=hf_abcdefghijklmnopqrst",
            "token=huggingface",
        );
    }

    #[test]
    fn replicate_token_matches_only_token_shape() {
        assert_pattern(
            "replicate_token",
            "token=r8_abcdefghijklmnopqrst",
            "token=replicate",
        );
    }

    #[test]
    fn elevenlabs_key_matches_only_key_shape() {
        assert_pattern(
            "elevenlabs_key",
            "key=abcdef0123456789abcdef0123456789",
            "key=elevenlabs",
        );
    }

    #[test]
    fn ssh_private_key_matches_only_private_pem() {
        assert_pattern(
            "ssh_private_key",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n-----END OPENSSH PRIVATE KEY-----",
            "-----BEGIN PUBLIC KEY-----\npublic\n-----END PUBLIC KEY-----",
        );
    }

    #[test]
    fn pem_certificate_matches_only_certificate_pem() {
        assert_pattern(
            "pem_certificate",
            "-----BEGIN CERTIFICATE-----\ncertificate\n-----END CERTIFICATE-----",
            "certificate text without PEM markers",
        );
    }

    #[test]
    fn connection_string_matches_only_database_urls() {
        assert_pattern(
            "connection_string",
            "postgres://user:password@db.example/database",
            "https://example.com/document",
        );
    }

    #[test]
    fn private_ip_matches_only_private_address() {
        assert_pattern("private_ip", "server=192.168.1.24", "server=8.8.8.8");
    }

    #[test]
    fn email_matches_only_address_shape() {
        assert_pattern(
            "email",
            "contact=alice@example.com",
            "contact=alice at example dot com",
        );
    }

    #[test]
    fn credit_card_matches_only_long_digit_shape() {
        assert_pattern("credit_card", "card=4111 1111 1111 1111", "card=1234-5678");
    }

    #[test]
    fn private_ip_pattern_covers_all_private_ranges() {
        let pattern = PATTERNS
            .iter()
            .find(|pattern| pattern.id == "private_ip")
            .unwrap();
        for address in ["10.0.0.1", "127.0.0.1", "172.16.0.1", "192.168.0.1"] {
            assert!(pattern.re.is_match(address), "did not match {address}");
        }
        assert!(!pattern.re.is_match("172.15.0.1"));
        assert!(!pattern.re.is_match("172.32.0.1"));
    }

    #[test]
    fn substitute_returns_correct_string_for_each_kind() {
        assert_eq!(
            substitute(PatternKind::SkCpApiKey),
            "***REDACTED:api_key:sk-cp***"
        );
        assert_eq!(
            substitute(PatternKind::GithubPat),
            "***REDACTED:github_pat***"
        );
        assert_eq!(
            substitute(PatternKind::AwsAccessKey),
            "***REDACTED:aws_access_key***"
        );
        assert_eq!(substitute(PatternKind::Jwt), "***REDACTED:jwt***");
        assert_eq!(
            substitute(PatternKind::BearerHeader),
            "Bearer ***REDACTED***"
        );
        assert_eq!(
            substitute(PatternKind::PasswordKv),
            "***REDACTED:password***"
        );
        assert_eq!(substitute(PatternKind::Email), "***REDACTED:email***");
        assert_eq!(substitute(PatternKind::PrivateIp), "***REDACTED:ip***");
        assert_eq!(substitute(PatternKind::CreditCard), "***REDACTED:cc***");
        assert_eq!(
            substitute(PatternKind::PrivateKey),
            "***REDACTED:private_key***"
        );
        assert_eq!(
            substitute(PatternKind::ConnString),
            "***REDACTED:connstring***"
        );
        assert_eq!(
            substitute(PatternKind::AnthropicApiKey),
            "***REDACTED:api_key:sk-ant***"
        );
        assert_eq!(
            substitute(PatternKind::OpenaiApiKey),
            "***REDACTED:api_key:sk***"
        );
        assert_eq!(
            substitute(PatternKind::GeminiApiKey),
            "***REDACTED:api_key:AIza***"
        );
        assert_eq!(substitute(PatternKind::Unknown), "***REDACTED***");
    }

    /// The serialised form is `snake_case` so the JSON sidecars
    /// stay human-friendly; the SQL audit row mirrors the same
    /// string.
    #[test]
    fn pattern_kind_serializes_to_snake_case() {
        let k = PatternKind::SkCpApiKey;
        let j = serde_json::to_string(&k).unwrap();
        assert_eq!(j, "\"sk_cp_api_key\"");
        let back: PatternKind = serde_json::from_str(&j).unwrap();
        assert_eq!(back, k);

        let k = PatternKind::AwsAccessKey;
        let j = serde_json::to_string(&k).unwrap();
        assert_eq!(j, "\"aws_access_key\"");
    }

    #[test]
    fn kind_for_pattern_id_maps_known_patterns() {
        assert_eq!(
            kind_for_pattern_id("minimax_sk_cp"),
            Some(PatternKind::SkCpApiKey)
        );
        assert_eq!(
            kind_for_pattern_id("openai_key"),
            Some(PatternKind::OpenaiApiKey)
        );
        assert_eq!(
            kind_for_pattern_id("anthropic_key"),
            Some(PatternKind::AnthropicApiKey)
        );
        assert_eq!(
            kind_for_pattern_id("gemini_key"),
            Some(PatternKind::GeminiApiKey)
        );
        assert_eq!(
            kind_for_pattern_id("github_pat"),
            Some(PatternKind::GithubPat)
        );
        assert_eq!(
            kind_for_pattern_id("github_oauth"),
            Some(PatternKind::GithubPat)
        );
        assert_eq!(
            kind_for_pattern_id("github_app"),
            Some(PatternKind::GithubPat)
        );
        assert_eq!(
            kind_for_pattern_id("aws_access_key"),
            Some(PatternKind::AwsAccessKey)
        );
        assert_eq!(
            kind_for_pattern_id("bearer"),
            Some(PatternKind::BearerHeader)
        );
        assert_eq!(kind_for_pattern_id("jwt"), Some(PatternKind::Jwt));
        assert_eq!(
            kind_for_pattern_id("pem_private_key"),
            Some(PatternKind::PrivateKey)
        );
        assert_eq!(kind_for_pattern_id("ip_v4"), Some(PatternKind::PrivateIp));
        assert_eq!(
            kind_for_pattern_id("credit_card"),
            Some(PatternKind::CreditCard)
        );
        assert_eq!(kind_for_pattern_id("email"), Some(PatternKind::Email));
        assert_eq!(kind_for_pattern_id("unknown_pattern"), None);
    }
}
