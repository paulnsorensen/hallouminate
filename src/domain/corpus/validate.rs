//! Non-blocking lint pass run on `add_markdown` content before it is written.
//!
//! The corpus stores the LLM's bytes verbatim, so this never rewrites or
//! rejects content — it walks the same `pulldown-cmark` event stream the
//! indexer uses and returns advisory warnings the MCP response carries back
//! in the same round-trip. Catching a broken link or empty diagram at write
//! time saves the read-discover-rewrite loop later.

use std::path::Path;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

/// Walk `content` and return one advisory message per detected issue. An empty
/// vec means nothing flagged. Pure and allocation-light; no I/O.
pub fn lint_markdown(content: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut last_heading: Option<usize> = None;
    let mut iter = Parser::new(content);

    while let Some(event) = iter.next() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let n = heading_num(level);
                if let Some(prev) = last_heading
                    && n > prev + 1
                {
                    warnings.push(format!(
                        "heading level jumps from h{prev} to h{n} (skips h{})",
                        prev + 1
                    ));
                }
                last_heading = Some(n);
            }
            Event::Start(Tag::Link { dest_url, .. }) if dest_url.trim().is_empty() => {
                let text = collect_link_text(&mut iter);
                warnings.push(format!("link \"{text}\" has an empty destination"));
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if info
                    .split_whitespace()
                    .next()
                    .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid")) =>
            {
                if collect_code_block(&mut iter).trim().is_empty() {
                    warnings.push("empty ```mermaid code block".to_string());
                }
            }
            _ => {}
        }
    }
    warnings
}

fn heading_num(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn collect_link_text(iter: &mut Parser<'_>) -> String {
    let mut buf = String::new();
    for event in iter.by_ref() {
        match event {
            Event::End(TagEnd::Link) => break,
            Event::Text(t) | Event::Code(t) => buf.push_str(&t),
            _ => {}
        }
    }
    buf.trim().to_string()
}

fn collect_code_block(iter: &mut Parser<'_>) -> String {
    let mut buf = String::new();
    for event in iter.by_ref() {
        match event {
            Event::End(TagEnd::CodeBlock) => break,
            Event::Text(t) => buf.push_str(&t),
            _ => {}
        }
    }
    buf
}

/// Extract the raw target text of every `[[wikilink]]` in `content`,
/// skipping fenced code blocks — a wikilink written inside an example is not
/// a real link. `[[wikilinks]]` are plain text to `pulldown-cmark` (only
/// `[text](url)` links parse as `Tag::Link`), so this scans `Event::Text`
/// directly rather than matching on link tags. `pulldown-cmark` can split a
/// single run of text into several adjacent `Text` events (e.g. `[[` and the
/// rest), so consecutive text is buffered and flushed on any non-text
/// boundary event rather than scanned event-by-event.
pub fn find_wikilinks(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut in_code_block = false;
    let mut buf = String::new();
    for event in Parser::new(content) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                if !buf.is_empty() {
                    extract_wikilinks(&buf, &mut links);
                    buf.clear();
                }
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Text(text) if !in_code_block => buf.push_str(&text),
            _ if !buf.is_empty() => {
                extract_wikilinks(&buf, &mut links);
                buf.clear();
            }
            _ => {}
        }
    }
    if !buf.is_empty() {
        extract_wikilinks(&buf, &mut links);
    }
    links
}

fn extract_wikilinks(text: &str, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(open) = rest.find("[[") {
        let after_open = &rest[open + 2..];
        let Some(close) = after_open.find("]]") else {
            break;
        };
        let inner = &after_open[..close];
        let target = inner.split('|').next().unwrap_or(inner).trim();
        if !target.is_empty() {
            out.push(target.to_string());
        }
        rest = &after_open[close + 2..];
    }
}

/// Normalize a wikilink target or corpus-relative path into a comparable
/// slug: lowercase, forward slashes, no `.md` extension.
pub fn normalize_slug(raw: &str) -> String {
    let lower = raw.trim().replace('\\', "/").to_lowercase();
    lower
        .strip_suffix(".md")
        .map(str::to_string)
        .unwrap_or(lower)
}
/// Result of resolving a wikilink target against a corpus's paths — shared
/// by `add_markdown`'s wikilink lint and `handle_backlinks`' bare-stem
/// uniqueness check so the two agree on whether a bare stem identifies one
/// page, no page, or several.
#[derive(Debug, PartialEq, Eq)]
pub enum SlugResolution {
    /// No page's full path or bare stem matches the target.
    Missing,
    /// Exactly one page matches; carries its corpus-relative path.
    Unique(String),
    /// More than one page shares the bare stem; carries every candidate.
    Ambiguous(Vec<String>),
}

/// Resolve `target` against every corpus-relative path in `paths`. A page
/// matches when its normalized full path equals `target`, or when its bare
/// filename stem does.
pub fn resolve_slug(target: &str, paths: &[String]) -> SlugResolution {
    let target_slug = normalize_slug(target);
    let matches: Vec<String> = paths
        .iter()
        .filter(|path| {
            normalize_slug(path) == target_slug
                || Path::new(path.as_str())
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .as_deref()
                    == Some(target_slug.as_str())
        })
        .cloned()
        .collect();
    match matches.len() {
        0 => SlugResolution::Missing,
        1 => SlugResolution::Unique(matches.into_iter().next().expect("len == 1")),
        _ => SlugResolution::Ambiguous(matches),
    }
}

/// Flag every `[[wikilink]]` in `content` whose target does not uniquely
/// resolve to a page in `paths`. Advisory-only, mirrors `lint_markdown`:
/// never rewrites or blocks the write. A bare stem shared by two or more
/// pages is flagged as ambiguous, listing the candidates, instead of being
/// silently accepted.
pub fn lint_wikilinks(content: &str, paths: &[String]) -> Vec<String> {
    find_wikilinks(content)
        .into_iter()
        .filter_map(|target| match resolve_slug(&target, paths) {
            SlugResolution::Unique(_) => None,
            SlugResolution::Missing => Some(format!(
                "wikilink [[{target}]] has no matching page in the corpus"
            )),
            SlugResolution::Ambiguous(candidates) => Some(format!(
                "wikilink [[{target}]] is ambiguous \u{2014} matches {}",
                candidates.join(", ")
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_document_has_no_warnings() {
        let text = "# Title\n\n## Section\n\nA [real link](https://example.com) and text.\n\n\
                    ```mermaid\ngraph TD\n  A --> B\n```\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn flags_empty_mermaid_block() {
        let text = "# Diagram\n\n```mermaid\n```\n";
        let warnings = lint_markdown(text);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("mermaid"));
    }

    #[test]
    fn mermaid_with_whitespace_only_body_is_empty() {
        let text = "```mermaid\n   \n\n```\n";
        assert_eq!(lint_markdown(text).len(), 1);
    }

    #[test]
    fn non_empty_mermaid_is_not_flagged() {
        let text = "```mermaid\nsequenceDiagram\n  A->>B: hi\n```\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn empty_fence_of_other_language_is_ignored() {
        // Only mermaid blocks are checked — an empty ```rust fence is the
        // author's business, not a diagram we can flag as broken.
        let text = "```rust\n```\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn mermaid_match_is_case_insensitive() {
        let text = "```Mermaid\n```\n";
        assert_eq!(lint_markdown(text).len(), 1);
    }

    #[test]
    fn flags_empty_destination_link() {
        let text = "See [the spec]() for details.\n";
        let warnings = lint_markdown(text);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("the spec"));
        assert!(warnings[0].contains("empty destination"));
    }

    #[test]
    fn link_with_fragment_destination_is_not_flagged() {
        let text = "Jump to [section](#install).\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn flags_heading_level_jump() {
        let text = "# Top\n\n### Skipped h2\n";
        let warnings = lint_markdown(text);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("h1 to h3"));
    }

    #[test]
    fn sequential_heading_levels_are_not_flagged() {
        let text = "# A\n\n## B\n\n### C\n\n## D\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn heading_decrease_is_not_flagged() {
        let text = "# A\n\n## B\n\n### C\n\n# E\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn first_heading_at_any_level_is_not_flagged() {
        // Starting at h3 is an MD041-style concern we deliberately do not flag;
        // only mid-document level *jumps* are.
        let text = "### Starts deep\n\n#### Deeper\n";
        assert!(lint_markdown(text).is_empty());
    }

    #[test]
    fn multiple_issues_accumulate() {
        let text = "# Title\n\n### Jump\n\n[bad]() link\n\n```mermaid\n```\n";
        let warnings = lint_markdown(text);
        assert_eq!(warnings.len(), 3, "warnings: {warnings:?}");
    }

    #[test]
    fn empty_content_has_no_warnings() {
        assert!(lint_markdown("").is_empty());
    }

    #[test]
    fn valid_wikilink_is_not_flagged() {
        let paths = vec!["guide/setup.md".to_string()];
        let text = "See [[guide/setup]] for details.\n";
        assert!(lint_wikilinks(text, &paths).is_empty());
    }

    #[test]
    fn broken_wikilink_is_flagged() {
        let paths = vec!["guide/setup.md".to_string()];
        let text = "See [[missing-page]] for details.\n";
        let warnings = lint_wikilinks(text, &paths);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("missing-page"));
    }

    #[test]
    fn wikilink_inside_code_fence_is_ignored() {
        let paths = vec!["guide/setup.md".to_string()];
        let text = "```text\n[[missing-page]]\n```\n";
        assert!(lint_wikilinks(text, &paths).is_empty());
    }

    #[test]
    fn ambiguous_bare_wikilink_lists_candidate_paths() {
        // Two pages share the bare stem "setup"; a bare `[[setup]]` wikilink
        // cannot be resolved to either one, matching `handle_backlinks`'
        // refusal to treat a colliding stem as identifying a single page.
        let paths = vec!["guide/setup.md".to_string(), "other/setup.md".to_string()];
        let text = "See [[setup]] for details.\n";
        let warnings = lint_wikilinks(text, &paths);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("ambiguous"));
        assert!(warnings[0].contains("guide/setup.md"));
        assert!(warnings[0].contains("other/setup.md"));
    }

    #[test]
    fn resolve_slug_agrees_with_backlinks_bare_stem_rule() {
        // `resolve_slug` is the shared resolver: a bare stem matching two
        // pages is Ambiguous, matching exactly one is Unique, and matching
        // none is Missing — the same three outcomes `handle_backlinks`
        // needs when deciding whether a stem uniquely names the page it is
        // looking up backlinks for.
        let paths = vec!["guide/setup.md".to_string(), "other/setup.md".to_string()];
        assert_eq!(
            resolve_slug("setup", &paths),
            SlugResolution::Ambiguous(vec![
                "guide/setup.md".to_string(),
                "other/setup.md".to_string()
            ])
        );

        let single = vec!["guide/setup.md".to_string()];
        assert_eq!(
            resolve_slug("setup", &single),
            SlugResolution::Unique("guide/setup.md".to_string())
        );

        assert_eq!(resolve_slug("nowhere", &single), SlugResolution::Missing);
    }
}
