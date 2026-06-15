//! Claim-level provenance marks (confirmed/qualified/superseded/contradicted).
//!
//! A claim mark is an inline HTML comment that tags an individual claim sentence
//! inside a wiki page, e.g. `<!--claim:superseded ref=path/to/page.md-->`. Unlike
//! page-level [`crate::domain::corpus::Frontmatter`] (denormalized identically
//! onto every chunk), claim marks are *positional*: each belongs to the chunk
//! whose line-range contains it.
//!
//! Marks are parsed at index time, stored as canonical JSON in a per-chunk Lance
//! column, surfaced in `ground` results, and flagged by an advisory linter. The
//! comment text is stripped from retrieval text (embeddings + snippets) while the
//! structured marks live in metadata, so the prose stays clean and the on-disk
//! file remains the verbatim source of truth.
//!
//! Parsing is fail-soft: an unrecognized `STATUS` is ignored (treated as an
//! ordinary HTML comment), mirroring `LifecycleStatus::from_str_ci` returning
//! `None`. Non-claim HTML comments are never consumed.

use serde::{Deserialize, Serialize};

/// Provenance status of a single claim. Parsed case-insensitively; an
/// unrecognized value yields `None` from [`ClaimStatus::from_str_ci`] (the
/// comment is then ignored as a claim mark) rather than erroring the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimStatus {
    Confirmed,
    Qualified,
    Superseded,
    Contradicted,
}

impl ClaimStatus {
    /// Case-insensitive parse. Returns `None` for any unrecognized value.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "confirmed" => Some(Self::Confirmed),
            "qualified" => Some(Self::Qualified),
            "superseded" => Some(Self::Superseded),
            "contradicted" => Some(Self::Contradicted),
            _ => None,
        }
    }

    /// True for the two statuses the linter expects to carry a `ref=` pointer.
    fn expects_reference(self) -> bool {
        matches!(self, Self::Superseded | Self::Contradicted)
    }
}

/// One parsed claim mark anchored to a body line.
///
/// Plain data: rides the wire in `ground` results and the Lance `claim_marks`
/// JSON column, so it derives serde with a stable field shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimMark {
    /// Provenance status of the claim.
    pub status: ClaimStatus,
    /// 1-indexed line the mark sits on. Body-relative as returned by
    /// [`extract_claim_marks`]; the indexer adds `fm_lines` for the on-disk
    /// citation.
    pub line: usize,
    /// Optional opaque pointer (`ref=`): a wiki path, footnote label, or URL.
    /// Recommended for `superseded`/`contradicted`; the linter only checks
    /// presence — no path/URL/footnote resolution.
    pub reference: Option<String>,
    /// Optional free-text rationale (`note="..."`).
    pub note: Option<String>,
}

/// The fields parsed out of a single claim comment's attribute span.
struct Attributes {
    reference: Option<String>,
    note: Option<String>,
}

/// Outcome of inspecting one HTML comment for claim-mark shape. Shared by
/// [`extract_claim_marks`] and [`lint_claim_marks`] so the two agree on what
/// counts as a (well-formed, malformed, or non-claim) comment.
enum Comment {
    /// Not a `<!--claim:...-->` comment at all (an ordinary HTML comment).
    NotClaim,
    /// A `claim:` comment whose status is unrecognized — ignored as a mark, but
    /// the linter flags it as malformed.
    Malformed,
    /// A well-formed claim mark.
    Mark {
        status: ClaimStatus,
        reference: Option<String>,
        note: Option<String>,
    },
}

/// Classify the inner text of an HTML comment (the bytes between `<!--` and
/// `-->`). `inner` excludes the delimiters.
fn classify(inner: &str) -> Comment {
    let trimmed = inner.trim();
    let Some(rest) = trimmed.strip_prefix("claim:") else {
        return Comment::NotClaim;
    };
    // `rest` is `STATUS [ref=... note="..."]`. The status token runs up to the
    // first ASCII whitespace; the remainder is the attribute span.
    let (status_tok, attr_span) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let Some(status) = ClaimStatus::from_str_ci(status_tok) else {
        return Comment::Malformed;
    };
    let Attributes { reference, note } = parse_attributes(attr_span);
    Comment::Mark {
        status,
        reference,
        note,
    }
}

/// Parse the optional `ref=<token>` and `note="quoted text"` attributes from the
/// span after the status token. Unknown tokens are ignored (the on-disk file
/// stays the source of truth). `ref` is opaque and unquoted (a bare token);
/// `note` is double-quoted free text.
fn parse_attributes(span: &str) -> Attributes {
    let mut reference = None;
    let mut note = None;
    let bytes = span.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if let Some(after) = span[i..].strip_prefix("note=\"") {
            // Free text up to the next double quote.
            let value_start = i + "note=\"".len();
            match after.find('"') {
                Some(rel_end) => {
                    note = Some(span[value_start..value_start + rel_end].to_string());
                    i = value_start + rel_end + 1;
                }
                None => {
                    // Unterminated quote: take the rest of the span as the note.
                    note = Some(span[value_start..].to_string());
                    break;
                }
            }
        } else if span[i..].starts_with("ref=") {
            let value_start = i + "ref=".len();
            // Opaque bare token: runs to the next whitespace.
            let rel_end = span[value_start..]
                .find(char::is_whitespace)
                .unwrap_or(span.len() - value_start);
            let value = &span[value_start..value_start + rel_end];
            if !value.is_empty() {
                reference = Some(value.to_string());
            }
            i = value_start + rel_end;
        } else {
            // Unknown token: skip to the next whitespace.
            let rel_end = span[i..]
                .find(char::is_whitespace)
                .unwrap_or(span.len() - i);
            i += rel_end.max(1);
        }
    }
    Attributes { reference, note }
}

/// Scan `content` line by line, yielding `(line_1_indexed, inner)` for every
/// HTML comment whose open and close delimiters sit on the same line. Claim
/// marks are single-line by grammar, so a multi-line `<!-- ... -->` is never a
/// claim mark and is left for the body untouched.
fn scan_comments(content: &str) -> impl Iterator<Item = (usize, &str)> {
    content.lines().enumerate().flat_map(|(idx, line)| {
        let mut found = Vec::new();
        let mut rest = line;
        let mut consumed = 0usize;
        while let Some(open) = rest.find("<!--") {
            let after_open = &rest[open + 4..];
            let Some(close) = after_open.find("-->") else {
                break;
            };
            let inner = &after_open[..close];
            found.push((idx + 1, inner));
            let advance = open + 4 + close + 3;
            consumed += advance;
            rest = &line[consumed..];
        }
        found.into_iter()
    })
}

/// Parse all claim-mark comments from a markdown body.
///
/// Lines are body-relative and 1-indexed; the caller adds `fm_lines` for on-disk
/// citations. Marks are returned in line order. An unrecognized `STATUS` is
/// ignored (not a mark), and non-claim HTML comments are never consumed.
pub fn extract_claim_marks(content: &str) -> Vec<ClaimMark> {
    let mut marks = Vec::new();
    for (line, inner) in scan_comments(content) {
        if let Comment::Mark {
            status,
            reference,
            note,
        } = classify(inner)
        {
            marks.push(ClaimMark {
                status,
                line,
                reference,
                note,
            });
        }
    }
    marks
}

/// Remove `<!--claim:...-->` comments from `text`, preserving newlines so the
/// line count is unchanged and citations stay valid. Ordinary HTML comments are
/// left intact. Only single-line claim comments are stripped (claim marks are
/// single-line by grammar).
pub fn strip_claim_marks(text: &str) -> String {
    // `lines()` drops a trailing newline; reattach the original terminator shape
    // by splitting on '\n' inclusively so line count and EOL bytes are preserved.
    let mut out = String::with_capacity(text.len());
    for segment in text.split_inclusive('\n') {
        let (body, eol) = match segment.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (segment, ""),
        };
        out.push_str(&strip_claim_comments_in_line(body));
        out.push_str(eol);
    }
    out
}

/// Strip every `<!--claim:...-->` comment from a single line, leaving non-claim
/// comments and surrounding text untouched.
fn strip_claim_comments_in_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find("<!--") {
        let after_open = &rest[open + 4..];
        let Some(close) = after_open.find("-->") else {
            break;
        };
        let inner = &after_open[..close];
        let full_end = open + 4 + close + 3;
        if matches!(classify(inner), Comment::NotClaim) {
            // Keep the whole non-claim comment and the text before it.
            out.push_str(&rest[..full_end]);
        } else {
            // Drop the claim/malformed-claim comment; keep the text before it.
            out.push_str(&rest[..open]);
        }
        rest = &rest[full_end..];
    }
    out.push_str(rest);
    out
}

/// Canonical JSON for a slice of marks — the Lance `claim_marks` column payload.
///
/// `None` when the slice is empty (the column is then null for that chunk). The
/// shape is fixed (declaration field order, `status` lowercased) so the storage
/// and surfacing layers can rely on a stable schema. Serialization of this
/// plain-data slice cannot fail.
pub fn marks_to_canonical_json(marks: &[ClaimMark]) -> Option<String> {
    if marks.is_empty() {
        return None;
    }
    Some(serde_json::to_string(marks).expect("ClaimMark is plain data; serialization cannot fail"))
}

/// Advisory lint over a body's claim comments.
///
/// Flags two things, never blocking the write:
/// - a `claim:` comment whose `STATUS` is unrecognized (malformed); and
/// - a `superseded`/`contradicted` mark missing a `ref=` pointer (presence
///   check only — `ref` is opaque, so no path/URL/footnote resolution).
pub fn lint_claim_marks(content: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    for (line, inner) in scan_comments(content) {
        match classify(inner) {
            Comment::NotClaim => {}
            Comment::Malformed => warnings.push(format!(
                "line {line}: claim comment has an unrecognized status; expected one of \
                 confirmed/qualified/superseded/contradicted (it was ignored as a claim mark)"
            )),
            Comment::Mark {
                status, reference, ..
            } => {
                if status.expects_reference() && reference.is_none() {
                    warnings.push(format!(
                        "line {line}: {} claim mark is missing a `ref=` pointer",
                        status_word(status)
                    ));
                }
            }
        }
    }
    warnings
}

/// The lowercase wire word for a status, reused by the linter message so the
/// advisory and the serialized JSON name the status identically.
fn status_word(status: ClaimStatus) -> &'static str {
    match status {
        ClaimStatus::Confirmed => "confirmed",
        ClaimStatus::Qualified => "qualified",
        ClaimStatus::Superseded => "superseded",
        ClaimStatus::Contradicted => "contradicted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_of_each_status_with_body_relative_lines() {
        let body = "# H\n\
                    confirmed claim.<!--claim:confirmed-->\n\
                    qualified claim.<!--claim:qualified-->\n\
                    superseded claim.<!--claim:superseded ref=old.md-->\n\
                    contradicted claim.<!--claim:contradicted ref=https://x/rfc-->\n";
        let marks = extract_claim_marks(body);
        assert_eq!(
            marks,
            vec![
                ClaimMark {
                    status: ClaimStatus::Confirmed,
                    line: 2,
                    reference: None,
                    note: None,
                },
                ClaimMark {
                    status: ClaimStatus::Qualified,
                    line: 3,
                    reference: None,
                    note: None,
                },
                ClaimMark {
                    status: ClaimStatus::Superseded,
                    line: 4,
                    reference: Some("old.md".into()),
                    note: None,
                },
                ClaimMark {
                    status: ClaimStatus::Contradicted,
                    line: 5,
                    reference: Some("https://x/rfc".into()),
                    note: None,
                },
            ]
        );
    }

    #[test]
    fn unknown_status_is_ignored_not_a_mark() {
        let body = "claim.<!--claim:bananas-->\nreal.<!--claim:confirmed-->\n";
        let marks = extract_claim_marks(body);
        assert_eq!(
            marks,
            vec![ClaimMark {
                status: ClaimStatus::Confirmed,
                line: 2,
                reference: None,
                note: None,
            }],
            "unknown status must not produce a mark"
        );
    }

    #[test]
    fn multiple_marks_on_distinct_lines_are_ordered_by_line() {
        let body = "a<!--claim:contradicted ref=z-->\n\
                    b<!--claim:confirmed-->\n\
                    c<!--claim:qualified-->\n";
        let lines: Vec<usize> = extract_claim_marks(body).iter().map(|m| m.line).collect();
        assert_eq!(lines, vec![1, 2, 3], "marks must be ordered by line");
    }

    #[test]
    fn multiple_marks_on_same_line_both_captured_in_order() {
        let body = "first<!--claim:confirmed--> then<!--claim:qualified-->\n";
        let marks = extract_claim_marks(body);
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].status, ClaimStatus::Confirmed);
        assert_eq!(marks[0].line, 1);
        assert_eq!(marks[1].status, ClaimStatus::Qualified);
        assert_eq!(marks[1].line, 1);
    }

    #[test]
    fn ref_and_note_both_parse() {
        let body =
            "x<!--claim:contradicted ref=https://example.com/rfc note=\"repealed in v3\"-->\n";
        let marks = extract_claim_marks(body);
        assert_eq!(
            marks[0].reference.as_deref(),
            Some("https://example.com/rfc")
        );
        assert_eq!(marks[0].note.as_deref(), Some("repealed in v3"));
    }

    #[test]
    fn note_only_parses_without_ref() {
        let body = "y<!--claim:qualified note=\"only on macOS\"-->\n";
        let marks = extract_claim_marks(body);
        assert_eq!(marks[0].reference, None);
        assert_eq!(marks[0].note.as_deref(), Some("only on macOS"));
    }

    #[test]
    fn note_with_spaces_and_internal_punctuation_is_captured_whole() {
        let body = "z<!--claim:qualified note=\"holds, mostly: on x86 only\"-->\n";
        let marks = extract_claim_marks(body);
        assert_eq!(
            marks[0].note.as_deref(),
            Some("holds, mostly: on x86 only"),
            "the entire quoted span is the note"
        );
    }

    #[test]
    fn status_is_case_insensitive() {
        for raw in ["CONFIRMED", "Confirmed", "cOnFiRmEd"] {
            let body = format!("x<!--claim:{raw}-->\n");
            let marks = extract_claim_marks(&body);
            assert_eq!(
                marks,
                vec![ClaimMark {
                    status: ClaimStatus::Confirmed,
                    line: 1,
                    reference: None,
                    note: None,
                }],
                "raw={raw:?}"
            );
        }
    }

    #[test]
    fn from_str_ci_returns_none_for_unknown() {
        assert_eq!(ClaimStatus::from_str_ci("draft"), None);
        assert_eq!(ClaimStatus::from_str_ci(""), None);
        assert_eq!(
            ClaimStatus::from_str_ci("  superseded  "),
            Some(ClaimStatus::Superseded)
        );
    }

    #[test]
    fn strip_removes_claim_comments_and_preserves_line_count() {
        let text = "intro<!--claim:confirmed-->\n\
                    middle line\n\
                    end<!--claim:superseded ref=x-->\n";
        let stripped = strip_claim_marks(text);
        assert_eq!(
            stripped, "intro\nmiddle line\nend\n",
            "claim comments removed, surrounding prose intact"
        );
        assert_eq!(
            stripped.lines().count(),
            text.lines().count(),
            "line count must be preserved so citations stay valid"
        );
        assert!(
            !stripped.contains("<!--claim:"),
            "no raw claim comment may remain"
        );
    }

    #[test]
    fn strip_leaves_ordinary_html_comments_intact() {
        let text = "before <!-- ordinary note --> after<!--claim:confirmed-->\n";
        let stripped = strip_claim_marks(text);
        assert_eq!(
            stripped, "before <!-- ordinary note --> after\n",
            "non-claim comments must survive; only the claim comment is removed"
        );
    }

    #[test]
    fn ordinary_comment_produces_no_marks_and_no_warnings() {
        let text = "body <!-- just a note -->\nmore <!-- revisit later -->\n";
        assert!(extract_claim_marks(text).is_empty());
        assert!(lint_claim_marks(text).is_empty());
        assert_eq!(
            strip_claim_marks(text),
            text,
            "no claim comment → unchanged"
        );
    }

    #[test]
    fn strip_preserves_crlf_and_missing_final_newline() {
        let text = "a<!--claim:confirmed-->\r\nb<!--claim:qualified-->";
        let stripped = strip_claim_marks(text);
        assert_eq!(
            stripped, "a\r\nb",
            "CRLF and the absent trailing newline are preserved"
        );
    }

    #[test]
    fn canonical_json_is_stable_shape_and_lowercase_status() {
        let marks = vec![ClaimMark {
            status: ClaimStatus::Superseded,
            line: 7,
            reference: Some("old.md".into()),
            note: Some("moved".into()),
        }];
        assert_eq!(
            marks_to_canonical_json(&marks).expect("non-empty"),
            r#"[{"status":"superseded","line":7,"reference":"old.md","note":"moved"}]"#
        );
    }

    #[test]
    fn canonical_json_none_on_empty_slice() {
        assert_eq!(marks_to_canonical_json(&[]), None);
    }

    #[test]
    fn canonical_json_round_trips_back_to_marks() {
        let marks = vec![
            ClaimMark {
                status: ClaimStatus::Confirmed,
                line: 1,
                reference: None,
                note: None,
            },
            ClaimMark {
                status: ClaimStatus::Contradicted,
                line: 4,
                reference: Some("https://x".into()),
                note: Some("nope".into()),
            },
        ];
        let json = marks_to_canonical_json(&marks).expect("non-empty");
        let back: Vec<ClaimMark> = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, marks);
    }

    #[test]
    fn lint_warns_on_superseded_missing_ref() {
        let warnings = lint_claim_marks("x<!--claim:superseded-->\n");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("superseded"), "{warnings:?}");
        assert!(warnings[0].contains("ref="), "{warnings:?}");
        assert!(warnings[0].contains("line 1"), "{warnings:?}");
    }

    #[test]
    fn lint_warns_on_contradicted_missing_ref() {
        let warnings = lint_claim_marks("x<!--claim:contradicted-->\n");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("contradicted"), "{warnings:?}");
    }

    #[test]
    fn lint_silent_when_superseded_has_ref() {
        assert!(
            lint_claim_marks("x<!--claim:superseded ref=old.md-->\n").is_empty(),
            "a ref present must clear the advisory"
        );
    }

    #[test]
    fn lint_does_not_require_ref_for_confirmed_or_qualified() {
        assert!(lint_claim_marks("a<!--claim:confirmed-->\n").is_empty());
        assert!(lint_claim_marks("b<!--claim:qualified-->\n").is_empty());
    }

    #[test]
    fn lint_warns_on_malformed_unknown_status() {
        let warnings = lint_claim_marks("x<!--claim:bananas-->\n");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("unrecognized status"), "{warnings:?}");
    }

    #[test]
    fn lint_silent_on_ordinary_comment() {
        assert!(lint_claim_marks("body <!-- not a claim -->\n").is_empty());
    }

    #[test]
    fn empty_status_is_malformed_no_mark_and_lint_warns() {
        // `<!--claim:-->` has an empty status token, which from_str_ci("") rejects
        // as unrecognized. This must: produce no mark, fire one lint warning
        // (malformed), and be stripped from the text (same as any malformed claim
        // comment — it's not a NotClaim, so strip_claim_comments_in_line drops it).
        let text = "intro<!--claim:-->\nmore\n";
        assert!(
            extract_claim_marks(text).is_empty(),
            "empty status must not produce a mark"
        );
        let warnings = lint_claim_marks(text);
        assert_eq!(
            warnings.len(),
            1,
            "empty status must warn as malformed: {warnings:?}"
        );
        assert!(
            warnings[0].contains("unrecognized status"),
            "warning must name the problem: {}",
            warnings[0]
        );
        let stripped = strip_claim_marks(text);
        assert!(
            !stripped.contains("<!--claim:"),
            "malformed claim comment must be stripped: {stripped:?}"
        );
        assert_eq!(
            stripped.lines().count(),
            text.lines().count(),
            "strip must preserve line count"
        );
    }

    #[test]
    fn unterminated_comment_produces_no_mark_no_warning_and_survives_strip() {
        // An HTML comment missing `-->` is never a claim mark by grammar (the
        // scanner only yields comments with both delimiters on the same line).
        // It must not produce a mark, must not lint-warn, and must be left
        // byte-for-byte intact by strip_claim_marks.
        let text = "text<!--claim:confirmed\nmore\n";
        assert!(
            extract_claim_marks(text).is_empty(),
            "unterminated comment must not yield a mark"
        );
        assert!(
            lint_claim_marks(text).is_empty(),
            "unterminated comment must not lint-warn"
        );
        assert_eq!(
            strip_claim_marks(text),
            text,
            "unterminated comment must survive strip unchanged"
        );
    }

    #[test]
    fn ref_with_empty_value_yields_none_reference() {
        // `ref=` followed immediately by whitespace (empty bare token) must not
        // set reference — the guard `if !value.is_empty()` ensures reference
        // stays None so downstream null-checks work correctly.
        let body = "x<!--claim:superseded ref= note=\"reason\"-->\n";
        let marks = extract_claim_marks(body);
        assert_eq!(marks.len(), 1, "must still produce a mark");
        assert_eq!(marks[0].status, ClaimStatus::Superseded);
        assert_eq!(
            marks[0].reference, None,
            "empty ref= must leave reference None"
        );
        assert_eq!(marks[0].note.as_deref(), Some("reason"));
        // The linter must warn because superseded with no ref=.
        let warnings = lint_claim_marks(body);
        assert_eq!(
            warnings.len(),
            1,
            "empty ref= is treated as absent: {warnings:?}"
        );
        assert!(warnings[0].contains("ref="), "{}", warnings[0]);
    }
}
