//! Machine-readable codes carried by [`Warning::code`](super::Warning::code).
//!
//! Every code a ground response can carry is declared here once. Producers
//! use these constants instead of string literals, and [`ALL`] lets docs and
//! tests check that nothing drifts.

/// Fallback ripgrep results for files with no indexed rows were returned.
pub const LEXICAL_FALLBACK: &str = "lexical-fallback";

/// More unindexed files exist than one fallback pass searches.
pub const LEXICAL_FALLBACK_TRUNCATED: &str = "lexical-fallback-truncated";

/// The fallback could not list or search the unindexed files.
pub const LEXICAL_FALLBACK_FAILED: &str = "lexical-fallback-failed";

/// The fallback ripgrep pass ran out of time.
pub const LEXICAL_FALLBACK_TIMEOUT: &str = "lexical-fallback-timeout";

/// The indexable-file scan for the fallback ran out of time.
pub const LEXICAL_FALLBACK_SCAN_TIMEOUT: &str = "lexical-fallback-scan-timeout";

/// The ripgrep signal found hits that resolved to no indexed chunk.
pub const RIPGREP_UNRESOLVED: &str = "ripgrep-unresolved";

/// Ripgrep emitted match events that the parser could not read.
pub const RIPGREP_UNPARSEABLE: &str = "ripgrep-unparseable";

/// The ripgrep pass failed; ranking continued without its signal.
pub const RIPGREP_FAILED: &str = "ripgrep-failed";

/// The ripgrep pass timed out; ranking continued without its signal.
pub const RIPGREP_TIMEOUT: &str = "ripgrep-timeout";

/// The cross-encoder rerank timed out; ranking fell back to fusion order.
pub const RERANK_TIMEOUT: &str = "rerank-timeout";

/// The cross-encoder is unavailable; ranking fell back to fusion order.
pub const CROSSENCODER_UNAVAILABLE: &str = "crossencoder-unavailable";

/// A cross-repo union resolved a name collision between repositories.
pub const CROSS_REPO_UNION: &str = "cross-repo-union";

/// A configured path outside the repository root was skipped.
pub const CONFIG_PATH_OUTSIDE_REPO: &str = "config-path-outside-repo";

/// Fewer files are indexed than the corpus selects.
pub const INDEX_COVERAGE: &str = "index-coverage";

/// Index reconciliation is pending, failed, degraded, or not watched.
pub const INDEX_RECONCILIATION: &str = "index-reconciliation";

/// Phrase matching hit a scan cap or the result limit.
pub const PHRASE_TRUNCATED: &str = "phrase-truncated";

/// A counts response lists fewer files than matched.
pub const COUNTS_TRUNCATED: &str = "counts-truncated";

/// Every warning code, in declaration order.
pub const ALL: &[&str] = &[
    LEXICAL_FALLBACK,
    LEXICAL_FALLBACK_TRUNCATED,
    LEXICAL_FALLBACK_FAILED,
    LEXICAL_FALLBACK_TIMEOUT,
    LEXICAL_FALLBACK_SCAN_TIMEOUT,
    RIPGREP_UNRESOLVED,
    RIPGREP_UNPARSEABLE,
    RIPGREP_FAILED,
    RIPGREP_TIMEOUT,
    RERANK_TIMEOUT,
    CROSSENCODER_UNAVAILABLE,
    CROSS_REPO_UNION,
    CONFIG_PATH_OUTSIDE_REPO,
    INDEX_COVERAGE,
    INDEX_RECONCILIATION,
    PHRASE_TRUNCATED,
    COUNTS_TRUNCATED,
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn all_has_no_duplicates() {
        let unique: BTreeSet<&str> = ALL.iter().copied().collect();
        assert_eq!(unique.len(), ALL.len(), "duplicate warning code in ALL");
    }
}
