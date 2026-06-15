//! Optional page-level YAML frontmatter (lifecycle + provenance).
//!
//! Hallouminate enforces no markdown schema, so frontmatter is *optional*: a
//! leading `---…---` block is parsed when present and stripped before the body
//! reaches the chunker, summary, and keyword passes — it must never pollute
//! retrieval. Absence is the normal case and every recognized field is
//! optional. Parsing is fail-soft: a malformed, unterminated, or non-mapping
//! block is treated as plain body content rather than rejecting the file, since
//! the corpus stores the author's bytes verbatim.
//!
//! Only recognized fields are read; unknown YAML keys are ignored (the file on
//! disk stays the source of truth). The parsed fields are serialized to a
//! single canonical JSON string that the storage layer denormalizes onto every
//! chunk row, where the downstream lint pass (E2) can consume them.

use serde::{Deserialize, Deserializer, Serialize};

/// Lifecycle state of a wiki page. Parsed case-insensitively; an unrecognized
/// value leaves [`Frontmatter::status`] `None` (E2 owns the "unknown status"
/// advisory) rather than failing the whole block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleStatus {
    Draft,
    Reviewed,
    Trusted,
    Deprecated,
}

impl LifecycleStatus {
    /// Case-insensitive parse. Returns `None` for any unrecognized value.
    fn from_str_ci(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "draft" => Some(Self::Draft),
            "reviewed" => Some(Self::Reviewed),
            "trusted" => Some(Self::Trusted),
            "deprecated" => Some(Self::Deprecated),
            _ => None,
        }
    }
}

/// Recognized frontmatter fields. Every field is optional — absence is normal.
///
/// `#[serde(default)]` at the container level means any missing key falls back
/// to its `Default`, so a partial block (or `{}`) parses cleanly. Unknown keys
/// are ignored by serde, keeping the on-disk file authoritative.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Frontmatter {
    /// Lifecycle state. An unrecognized value parses the block but leaves this
    /// `None`.
    #[serde(deserialize_with = "deserialize_status")]
    pub status: Option<LifecycleStatus>,
    /// Free-form owner (person/team). Stored as-is.
    pub owner: Option<String>,
    /// ISO date the page was last verified. Stored verbatim; value-checked by E2.
    pub last_verified: Option<String>,
    /// Free-form confidence label for now; E2 may constrain it.
    pub confidence: Option<String>,
    /// Provenance source list; empty when absent.
    pub sources: Vec<String>,
}

impl Frontmatter {
    /// Serialize the recognized fields to a canonical JSON string.
    ///
    /// The shape is fixed (all five keys, declaration order, `status`
    /// lowercased, `None` → `null`) so the downstream lint (E2) can rely on a
    /// stable schema. Serialization of this plain-data struct cannot fail.
    pub fn to_canonical_json(&self) -> String {
        serde_json::to_string(self).expect("Frontmatter is plain data; serialization cannot fail")
    }
}

/// Custom deserializer for `status`: reads an optional raw string and maps it
/// case-insensitively, yielding `None` for unknown values instead of erroring.
fn deserialize_status<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<LifecycleStatus>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.and_then(|s| LifecycleStatus::from_str_ci(&s)))
}

/// Outcome of inspecting a body for a leading frontmatter block. Shared by
/// [`split_frontmatter`] and [`lint_frontmatter`] so the two agree on what
/// counts as frontmatter.
enum Block<'a> {
    /// No `---` fence on line 1, or an opening fence with no closing fence.
    /// Treated as plain content — not frontmatter.
    Absent,
    /// A delimited `---…---` block whose contents are not valid YAML for the
    /// recognized schema.
    Malformed,
    /// A delimited block that parsed. `rest` is the body after the closing
    /// fence; `lines` is the number of physical lines the block occupied
    /// (opening fence through closing fence inclusive).
    Parsed {
        fm: Frontmatter,
        rest: &'a str,
        lines: usize,
    },
}

/// Detect and parse a leading frontmatter block.
///
/// The opening fence must be exactly `---` on line 1 and the closing fence
/// exactly `---` (matching `read_h1`'s rule); a mid-document `---` is a thematic
/// break, never frontmatter. Byte offsets are tracked so `rest` is a real slice
/// of `body` (no reallocation), which keeps chunk line numbers mappable.
fn parse_block(body: &str) -> Block<'_> {
    let mut parts = body.split_inclusive('\n');

    let Some(first) = parts.next() else {
        return Block::Absent;
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Block::Absent;
    }

    let mut offset = first.len();
    let yaml_start = offset;
    let mut lines = 1usize; // counts the opening fence
    let mut yaml_end = None;
    for line in parts {
        lines += 1;
        if line.trim_end_matches(['\r', '\n']) == "---" {
            yaml_end = Some(offset);
            offset += line.len();
            break;
        }
        offset += line.len();
    }

    let Some(yaml_end) = yaml_end else {
        // Unterminated: an opening `---` with no close is ambiguous (likely a
        // thematic break), so treat the whole body as content.
        return Block::Absent;
    };

    let yaml = &body[yaml_start..yaml_end];
    let rest = &body[offset..];

    // An empty block (`---\n---`) is valid frontmatter carrying no fields.
    if yaml.trim().is_empty() {
        return Block::Parsed {
            fm: Frontmatter::default(),
            rest,
            lines,
        };
    }

    match serde_yaml_ng::from_str::<Frontmatter>(yaml) {
        Ok(fm) => Block::Parsed { fm, rest, lines },
        Err(_) => Block::Malformed,
    }
}

/// Split a leading `---\n…\n---` block from `body`.
///
/// Returns `(parsed, body_without_frontmatter, frontmatter_line_count)`.
/// Fail-soft: no leading block, an unterminated block, or unparseable YAML all
/// yield `(None, body, 0)` — the original body is left intact so it is indexed
/// verbatim. This never errors; indexing must not reject content.
///
/// The returned line count is the number of physical lines the block occupied,
/// so the caller can add it back to each chunk's line numbers and keep
/// citations pointing at the correct on-disk source lines.
pub fn split_frontmatter(body: &str) -> (Option<Frontmatter>, &str, usize) {
    match parse_block(body) {
        Block::Parsed { fm, rest, lines } => (Some(fm), rest, lines),
        Block::Absent | Block::Malformed => (None, body, 0),
    }
}

/// Advisory lint over a body's frontmatter block.
///
/// Stub seam for E2: returns exactly one advisory when a *delimited* `---…---`
/// block is present but its contents are not valid YAML for the recognized
/// schema. Well-formed, absent, or merely unterminated blocks add no warning.
/// E2 extends this with value-level checks (unknown status, malformed date,
/// unknown confidence).
pub fn lint_frontmatter(body: &str) -> Vec<String> {
    match parse_block(body) {
        Block::Malformed => vec![
            "frontmatter block present but not valid YAML; it was indexed as body text \
             (fix the leading `---` block or remove it)"
                .to_string(),
        ],
        Block::Absent | Block::Parsed { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_leading_block_returns_body_unchanged() {
        let body = "# Heading\n\nbody text\n";
        let (fm, rest, lines) = split_frontmatter(body);
        assert!(fm.is_none());
        assert_eq!(rest, body);
        assert_eq!(lines, 0);
    }

    #[test]
    fn mid_document_thematic_break_is_not_frontmatter() {
        let body = "# Heading\n\n---\n\nmore\n";
        let (fm, rest, lines) = split_frontmatter(body);
        assert!(fm.is_none());
        assert_eq!(rest, body);
        assert_eq!(lines, 0);
    }

    #[test]
    fn parses_recognized_fields_and_strips_block() {
        let body = "---\nstatus: reviewed\nowner: cheese-lord\nlast_verified: 2026-01-02\nconfidence: high\nsources:\n  - https://example.com/a\n  - https://example.com/b\n---\n# Heading\n\nbody\n";
        let (fm, rest, lines) = split_frontmatter(body);
        let fm = fm.expect("block parses");
        assert_eq!(fm.status, Some(LifecycleStatus::Reviewed));
        assert_eq!(fm.owner.as_deref(), Some("cheese-lord"));
        assert_eq!(fm.last_verified.as_deref(), Some("2026-01-02"));
        assert_eq!(fm.confidence.as_deref(), Some("high"));
        assert_eq!(
            fm.sources,
            vec!["https://example.com/a", "https://example.com/b"]
        );
        assert_eq!(rest, "# Heading\n\nbody\n");
        // opening fence + 7 content lines + closing fence = 9 physical lines.
        assert_eq!(lines, 9);
    }

    #[test]
    fn line_count_lets_caller_recover_on_disk_line_numbers() {
        // The heading sits on physical line 5 (1:---, 2:status, 3:owner, 4:---, 5:# H).
        let body = "---\nstatus: draft\nowner: x\n---\n# H\n";
        let (_, rest, lines) = split_frontmatter(body);
        assert_eq!(lines, 4);
        // The stripped body's first line is the heading; adding `lines` to its
        // 1-indexed position (1) recovers the on-disk line (5).
        assert_eq!(rest.lines().next(), Some("# H"));
        assert_eq!(1 + lines, 5);
    }

    #[test]
    fn unknown_keys_are_ignored_but_block_still_parses() {
        let body = "---\nunknown_key: whatever\nanother: 3\n---\nbody\n";
        let (fm, rest, lines) = split_frontmatter(body);
        let fm = fm.expect("unknown-only block still parses as frontmatter");
        assert_eq!(fm, Frontmatter::default());
        assert_eq!(rest, "body\n");
        assert_eq!(lines, 4);
    }

    #[test]
    fn unrecognized_status_value_parses_block_with_none_status() {
        let body = "---\nstatus: bananas\nowner: y\n---\nbody\n";
        let (fm, _, _) = split_frontmatter(body);
        let fm = fm.expect("block parses despite unknown status");
        assert_eq!(fm.status, None);
        assert_eq!(fm.owner.as_deref(), Some("y"));
    }

    #[test]
    fn status_parses_case_insensitively() {
        for raw in ["DRAFT", "Draft", "dRaFt", "  draft  "] {
            let body = format!("---\nstatus: {raw}\n---\nbody\n");
            let (fm, _, _) = split_frontmatter(&body);
            assert_eq!(
                fm.expect("parses").status,
                Some(LifecycleStatus::Draft),
                "raw={raw:?}"
            );
        }
    }

    #[test]
    fn malformed_yaml_is_fail_soft_and_left_in_body() {
        // A delimited block whose contents are not a YAML mapping.
        let body = "---\n: : : not valid : :\n  - dangling\n---\n# H\n";
        let (fm, rest, lines) = split_frontmatter(body);
        assert!(fm.is_none(), "malformed YAML yields no frontmatter");
        assert_eq!(rest, body, "body is left intact so it indexes verbatim");
        assert_eq!(lines, 0);
    }

    #[test]
    fn yaml_sequence_is_malformed_for_the_mapping_schema() {
        let body = "---\n- a\n- b\n---\nbody\n";
        let (fm, rest, _) = split_frontmatter(body);
        assert!(fm.is_none());
        assert_eq!(rest, body);
    }

    #[test]
    fn unterminated_block_is_treated_as_content() {
        let body = "---\nstatus: draft\nno closing fence here\n";
        let (fm, rest, lines) = split_frontmatter(body);
        assert!(fm.is_none());
        assert_eq!(rest, body);
        assert_eq!(lines, 0);
    }

    #[test]
    fn empty_block_parses_to_default_and_strips() {
        let body = "---\n---\n# H\n";
        let (fm, rest, lines) = split_frontmatter(body);
        assert_eq!(fm, Some(Frontmatter::default()));
        assert_eq!(rest, "# H\n");
        assert_eq!(lines, 2);
    }

    #[test]
    fn body_that_is_only_frontmatter_yields_empty_rest() {
        let body = "---\nstatus: trusted\n---\n";
        let (fm, rest, lines) = split_frontmatter(body);
        assert_eq!(fm.expect("parses").status, Some(LifecycleStatus::Trusted));
        assert_eq!(rest, "");
        assert_eq!(lines, 3);
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let body = "---\r\nstatus: draft\r\n---\r\n# H\r\n";
        let (fm, rest, lines) = split_frontmatter(body);
        assert_eq!(fm.expect("parses").status, Some(LifecycleStatus::Draft));
        assert_eq!(rest, "# H\r\n");
        assert_eq!(lines, 3);
    }

    #[test]
    fn lone_fence_with_no_newline_is_content() {
        let (fm, rest, lines) = split_frontmatter("---");
        assert!(fm.is_none());
        assert_eq!(rest, "---");
        assert_eq!(lines, 0);
    }

    #[test]
    fn canonical_json_has_fixed_shape_and_lowercase_status() {
        let fm = Frontmatter {
            status: Some(LifecycleStatus::Deprecated),
            owner: Some("team".into()),
            last_verified: None,
            confidence: None,
            sources: vec!["s1".into()],
        };
        assert_eq!(
            fm.to_canonical_json(),
            r#"{"status":"deprecated","owner":"team","last_verified":null,"confidence":null,"sources":["s1"]}"#
        );
    }

    #[test]
    fn canonical_json_round_trips_default() {
        let json = Frontmatter::default().to_canonical_json();
        assert_eq!(
            json,
            r#"{"status":null,"owner":null,"last_verified":null,"confidence":null,"sources":[]}"#
        );
    }

    #[test]
    fn lint_warns_only_on_malformed_block() {
        assert!(lint_frontmatter("# just a body\n").is_empty(), "absent");
        assert!(
            lint_frontmatter("---\nstatus: draft\n---\nbody\n").is_empty(),
            "well-formed"
        );
        assert!(
            lint_frontmatter("---\nopen but never closed\n").is_empty(),
            "unterminated → no warning"
        );
        let warnings = lint_frontmatter("---\n: : : not valid : :\n---\nbody\n");
        assert_eq!(warnings.len(), 1, "malformed → exactly one advisory");
        assert!(warnings[0].contains("frontmatter"), "{warnings:?}");
    }
}
