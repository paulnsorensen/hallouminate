//! Spec §8.1 #9: chunker budget compliance with the actual BGE-small
//! tokenizer. `#[ignore]`-gated because it downloads ~few-MB of tokenizer
//! files from HuggingFace on first run; opt-in via `cargo test -- --ignored`.

use hallouminate::domain::corpus::{load_tokenizer, MarkdownChunker};
use tokenizers::Tokenizer;

const BGE_MODEL: &str = "BAAI/bge-small-en-v1.5";
const BGE_BUDGET_TOKENS: usize = 480; // 90% cushion under BGE-small's 512 max

#[test]
#[ignore = "downloads tokenizer files on first run; opt-in via --ignored"]
fn chunker_with_real_bge_tokenizer_respects_token_budget() {
    let tok: Tokenizer = load_tokenizer(BGE_MODEL).expect("load BGE tokenizer");
    let chunker = MarkdownChunker::new(tok.clone(), BGE_BUDGET_TOKENS);

    // A markdown document large enough to force splitting on the BGE budget.
    let big_text = include_str!("fixtures/large.md");
    let chunks = chunker.chunk(big_text);
    assert!(!chunks.is_empty(), "must produce at least one chunk");

    for (i, c) in chunks.iter().enumerate() {
        let encoded = tok
            .encode(c.text.clone(), false)
            .expect("tokenize chunk for verification");
        assert!(
            encoded.len() <= BGE_BUDGET_TOKENS,
            "chunk {i} exceeded budget: {} tokens > {}\n--text--\n{}",
            encoded.len(),
            BGE_BUDGET_TOKENS,
            c.text
        );
    }
}

#[test]
#[ignore = "downloads tokenizer files on first run; opt-in via --ignored"]
fn chunker_with_real_bge_tokenizer_preserves_headings() {
    let tok = load_tokenizer(BGE_MODEL).expect("load BGE tokenizer");
    let chunker = MarkdownChunker::new(tok, BGE_BUDGET_TOKENS);
    let text = "# Top Section\n\nintro paragraph.\n\n## Subsection\n\nmore content.\n";
    let chunks = chunker.chunk(text);
    assert!(!chunks.is_empty());
    // first chunk should carry the H1 in its heading_path
    assert!(
        chunks
            .iter()
            .any(|c| c.heading_path.contains(&"Top Section".to_string())),
        "no chunk carried the H1 heading path: {:?}",
        chunks
            .iter()
            .map(|c| c.heading_path.clone())
            .collect::<Vec<_>>()
    );
}
