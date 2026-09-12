//! Asserts the pilot benchmark protocol document exists, carries every
//! required section in order, and states the facts the harness depends on
//! (usage field names, pass@k/pass^k definitions, minimum run count).

use std::path::Path;

const REQUIRED_HEADINGS: &[&str] = &[
    "## Goal and claim under test",
    "## Arms",
    "## Subject repos",
    "## Wiki authoring protocol",
    "## Question authoring protocol",
    "## Runs and metrics",
    "## Token accounting",
    "## Judging",
    "## Pinning and reproduction",
    "## Invalidation rules",
];

fn readme() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/agent-bench/README.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

fn headings(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with("## "))
        .collect()
}

#[test]
fn all_required_headings_are_present() {
    let text = readme();
    let present = headings(&text);
    let missing: Vec<&&str> = REQUIRED_HEADINGS
        .iter()
        .filter(|heading| !present.contains(heading))
        .collect();
    assert!(
        missing.is_empty(),
        "README is missing required headings: {missing:?}"
    );
}

#[test]
fn required_headings_appear_in_order() {
    let text = readme();
    let present = headings(&text);
    let ordered: Vec<&str> = present
        .into_iter()
        .filter(|h| REQUIRED_HEADINGS.contains(h))
        .collect();
    assert_eq!(
        ordered, REQUIRED_HEADINGS,
        "required headings must appear in the specified order"
    );
}

#[test]
fn token_usage_field_names_appear_verbatim() {
    let text = readme();
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ] {
        assert!(
            text.contains(field),
            "README must mention the usage field `{field}` verbatim"
        );
    }
}

#[test]
fn pass_at_k_and_pass_pow_k_are_both_defined() {
    let text = readme();
    assert!(text.contains("pass@k"), "README must define pass@k");
    assert!(text.contains("pass^k"), "README must define pass^k");
}

#[test]
fn minimum_runs_per_task_arm_is_stated() {
    let text = readme();
    assert!(
        text.contains("10 runs") || text.contains("≥10 runs"),
        "README must state the minimum-runs-per-task-arm figure of 10"
    );
}
