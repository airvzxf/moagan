//! External research fetcher (K.4 / proposal-04 §4).
//!
//! Narrower than the full proposal: 4 allowlist hosts only
//! ([`docs.rs`](allowlist::ALLOWED_HOSTS), `crates.io`,
//! `api.github.com`, `github.com`), [`fetcher::MAX_URLS_PER_CALL`]
//! URLs per call, [`fetcher::MAX_BYTES_PER_URL`] bytes per
//! response. Designed to ground proposals in current docs without
//! bloating token budget.
//!
//! Wire-up to the Sketch phase lands in a follow-up PR; the
//! intended slot is the `${known_apis}` placeholder so called
//! snippets augment the prompt rather than replace it.

pub mod allowlist;
pub mod fetcher;

pub use allowlist::ALLOWED_HOSTS;
pub use fetcher::{
    FetchError, MAX_BYTES_PER_URL, MAX_URLS_PER_CALL, ResearchFetcher, ResearchSnippet,
};

/// Backwards-compat free function that fetches with an empty API
/// key (no Authorization header on any host). K.4 follow-up work
/// should construct a [`ResearchFetcher`] explicitly so the
/// bearer opt-in hosts see the key when configured.
pub async fn fetch_all(
    urls: &[String],
) -> Vec<std::result::Result<ResearchSnippet, FetchError>> {
    ResearchFetcher::new(None).fetch_all(urls).await
}

/// Render a list of research snippets into the Markdown block that
/// stands in for the `${known_apis}` placeholder. Each snippet is
/// fenced with a `Source: <url>` heading so the model can cite it
/// and an explicit `truncated` flag when the fetcher hit the byte
/// cap. An empty list collapses to a single "no research available"
/// marker so the prompt never has the placeholder literally in it.
pub fn render_known_apis_block(snippets: &[ResearchSnippet]) -> String {
    if snippets.is_empty() {
        return "<!-- known_apis: no research available -->".to_owned();
    }
    let mut out = String::new();
    for (i, snippet) in snippets.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("### Source: {}\n", snippet.url));
        if snippet.truncated {
            out.push_str("(truncated to MAX_BYTES_PER_URL)\n");
        }
        out.push_str("```\n");
        out.push_str(snippet.content.trim());
        out.push_str("\n```\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_known_apis_block_empty_collapses_to_marker() {
        let s = render_known_apis_block(&[]);
        assert!(
            s.contains("no research available"),
            "empty input must surface the no-research marker, got {s:?}"
        );
    }

    #[test]
    fn render_known_apis_block_single_snippet_renders_source_heading() {
        let snippet = ResearchSnippet {
            url: "https://docs.rs/rust".into(),
            content: "fn main() {}".into(),
            truncated: false,
        };
        let s = render_known_apis_block(&[snippet]);
        assert!(s.contains("### Source: https://docs.rs/rust"));
        assert!(s.contains("fn main() {}"));
        assert!(
            !s.contains("truncated"),
            "non-truncated snippet must not flag"
        );
    }

    #[test]
    fn render_known_apis_block_truncated_snippet_is_flagged() {
        let snippet = ResearchSnippet {
            url: "https://crates.io/serde".into(),
            content: "truncated".into(),
            truncated: true,
        };
        let s = render_known_apis_block(&[snippet]);
        assert!(s.contains("(truncated to MAX_BYTES_PER_URL)"));
    }
}
