//! Asserts the opt-in bench-* justfile recipes exist, are documented, guard
//! every cost-bearing run with validator invocations before the runner
//! binary starts, and stay off the `ci` dependency/body graph.

use std::collections::BTreeSet;
use std::path::Path;

/// One `name ...params...:` recipe block: its preceding doc comment (if any)
/// and its indented body lines (blank lines dropped).
struct Recipe {
    doc: Option<String>,
    body: Vec<String>,
}

fn justfile_text() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../justfile");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

/// A top-level (unindented) line that declares a recipe: not blank, not a
/// comment, not a `set ...` directive, not a `name := value` assignment, and
/// containing a `:` that isn't part of `:=`.
fn is_recipe_header(line: &str) -> bool {
    if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    if line.starts_with('#') || line.starts_with("set ") {
        return false;
    }
    if line.contains(":=") {
        return false;
    }
    line.contains(':')
}

fn recipe_name(header: &str) -> &str {
    let end = header
        .find(|c: char| c == ':' || c.is_whitespace())
        .unwrap_or(header.len());
    &header[..end]
}

/// Parse every recipe block into a name -> Recipe map.
fn parse_recipes(text: &str) -> std::collections::BTreeMap<String, Recipe> {
    let lines: Vec<&str> = text.lines().collect();
    let mut recipes = std::collections::BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_recipe_header(line) {
            let name = recipe_name(line).to_string();
            let doc = if i > 0 && lines[i - 1].trim_start().starts_with('#') {
                Some(
                    lines[i - 1]
                        .trim_start()
                        .trim_start_matches('#')
                        .trim()
                        .to_string(),
                )
            } else {
                None
            };
            let mut body = Vec::new();
            let mut j = i + 1;
            while j < lines.len() && (lines[j].starts_with(' ') || lines[j].starts_with('\t')) {
                if !lines[j].trim().is_empty() {
                    body.push(lines[j].to_string());
                }
                j += 1;
            }
            recipes.insert(name, Recipe { doc, body });
            i = j;
        } else {
            i += 1;
        }
    }
    recipes
}

const BENCH_RECIPES: &[&str] = &[
    "bench-validate",
    "bench-author",
    "bench-run",
    "bench-judge",
    "bench-judge-calibrate",
    "bench-report",
];

/// Recipes that consume a frozen question set and so must require an
/// explicit `questions` argument: no hardcoded example paths, no default
/// value. `bench-author` is deliberately absent — authoring runs BEFORE the
/// question set exists (see `bench_author_does_not_gate_on_a_frozen_question_set`).
const QUESTION_SET_RECIPES: &[&str] = &[
    "bench-run",
    "bench-judge",
    "bench-judge-calibrate",
    "bench-report",
];

/// The five cost-bearing recipes (all of `BENCH_RECIPES` except the free
/// `bench-validate`) that must require an explicit manifest: no hardcoded
/// example paths, no default value.
const COST_BEARING_DATASET_RECIPES: &[&str] = &[
    "bench-author",
    "bench-run",
    "bench-judge",
    "bench-judge-calibrate",
    "bench-report",
];

/// Token-spending recipe -> the `--bin <runner>` marker its runner
/// invocation carries, distinct from the two validator markers.
const TOKEN_SPENDING_RECIPES: &[(&str, &str)] = &[
    ("bench-author", "--bin bench-author"),
    ("bench-run", "--bin bench-run"),
    ("bench-judge", "--bin bench-judge"),
    ("bench-judge-calibrate", "--bin bench-judge"),
];

const VALIDATOR_MANIFEST_MARKER: &str = "--bin bench-validate-manifest";
const VALIDATOR_QUESTIONS_MARKER: &str = "--bin bench-validate-questions";

#[test]
fn all_six_bench_recipes_are_present_and_documented() {
    let recipes = parse_recipes(&justfile_text());
    for name in BENCH_RECIPES {
        let recipe = recipes
            .get(*name)
            .unwrap_or_else(|| panic!("missing bench recipe `{name}`"));
        let doc = recipe
            .doc
            .as_ref()
            .unwrap_or_else(|| panic!("bench recipe `{name}` has no preceding doc comment"));
        assert!(
            !doc.is_empty(),
            "bench recipe `{name}`'s doc comment is empty"
        );
    }
}

#[test]
fn token_spending_recipes_invoke_validators_before_the_runner() {
    let recipes = parse_recipes(&justfile_text());
    for (name, runner_marker) in TOKEN_SPENDING_RECIPES {
        let recipe = recipes
            .get(*name)
            .unwrap_or_else(|| panic!("missing bench recipe `{name}`"));
        let body = recipe.body.join("\n");

        let manifest_pos = body
            .find(VALIDATOR_MANIFEST_MARKER)
            .unwrap_or_else(|| panic!("recipe `{name}` never invokes {VALIDATOR_MANIFEST_MARKER}"));
        let runner_pos = body
            .find(runner_marker)
            .unwrap_or_else(|| panic!("recipe `{name}` never invokes {runner_marker}"));

        assert!(
            manifest_pos < runner_pos,
            "recipe `{name}` must invoke {VALIDATOR_MANIFEST_MARKER} before {runner_marker}"
        );

        if QUESTION_SET_RECIPES.contains(name) {
            let questions_pos = body.find(VALIDATOR_QUESTIONS_MARKER).unwrap_or_else(|| {
                panic!("recipe `{name}` never invokes {VALIDATOR_QUESTIONS_MARKER}")
            });
            assert!(
                questions_pos < runner_pos,
                "recipe `{name}` must invoke {VALIDATOR_QUESTIONS_MARKER} before {runner_marker}"
            );
        }
    }
}

/// The protocol freezes the wiki FIRST and authors questions SECOND.
/// `bench-validate-questions` enforces a ≥24-question count and per-repo tag
/// floors, so gating `bench-author` on it makes a complete frozen question
/// set a prerequisite for authoring the wiki those questions are written
/// against — the ordering inverted.
#[test]
fn bench_author_does_not_gate_on_a_frozen_question_set() {
    let recipes = parse_recipes(&justfile_text());
    let body = recipes
        .get("bench-author")
        .expect("missing bench recipe `bench-author`")
        .body
        .join("\n");
    assert!(
        !body.contains(VALIDATOR_QUESTIONS_MARKER),
        "recipe `bench-author` must not invoke {VALIDATOR_QUESTIONS_MARKER}: \
         authoring consumes no question set, and the validator's count/tag \
         floors would require a frozen question set to exist before the wiki \
         those questions describe: {body}"
    );
}

/// Every non-defaulted parameter a bench recipe demands must actually be
/// used by its body. A required-but-discarded parameter makes callers pass
/// an argument that changes nothing — e.g. a `manifest` the recipe never
/// validates against.
#[test]
fn every_required_bench_recipe_param_is_used_by_its_body() {
    let text = justfile_text();
    let headers = recipe_headers(&text);
    let recipes = parse_recipes(&text);
    for name in BENCH_RECIPES {
        let header = headers
            .get(*name)
            .unwrap_or_else(|| panic!("missing bench recipe `{name}`"));
        let params = header
            .split(':')
            .next()
            .expect("header has a colon")
            .split_whitespace()
            .skip(1);
        let body = recipes[*name].body.join("\n");
        for param in params {
            if param.contains('=') {
                continue;
            }
            let interpolation = format!("{{{{{param}}}}}");
            assert!(
                body.contains(&interpolation),
                "recipe `{name}` requires parameter `{param}` but its body never \
                 uses {interpolation} — drop the parameter or wire it in: {body}"
            );
        }
    }
}

#[test]
fn token_spending_recipes_print_a_cost_warning() {
    let recipes = parse_recipes(&justfile_text());
    for (name, _) in TOKEN_SPENDING_RECIPES {
        let recipe = recipes
            .get(*name)
            .unwrap_or_else(|| panic!("missing bench recipe `{name}`"));
        let warning_line = recipe.body.iter().find(|line| {
            let trimmed = line.trim_start().trim_start_matches('@');
            trimmed.starts_with("echo") && trimmed.to_lowercase().contains("cost-bearing")
        });
        assert!(
            warning_line.is_some(),
            "recipe `{name}` must print a cost-bearing warning before running anything"
        );

        let warning_pos = recipe
            .body
            .iter()
            .position(|line| Some(line) == warning_line)
            .unwrap();
        let first_validator_line = recipe
            .body
            .iter()
            .position(|line| line.contains(VALIDATOR_MANIFEST_MARKER))
            .unwrap_or_else(|| panic!("recipe `{name}` never invokes {VALIDATOR_MANIFEST_MARKER}"));
        assert!(
            warning_pos < first_validator_line,
            "recipe `{name}` must print its cost warning before invoking the validators"
        );
    }
}

/// Walk `ci`'s body, following `just <recipe>` references transitively
/// (guarding against cycles), and collect every line text reachable from it.
fn ci_reachable_lines(recipes: &std::collections::BTreeMap<String, Recipe>) -> Vec<String> {
    let mut visited = BTreeSet::new();
    let mut lines = Vec::new();
    let mut queue = vec!["ci".to_string()];
    while let Some(name) = queue.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(recipe) = recipes.get(&name) else {
            continue;
        };
        for line in &recipe.body {
            lines.push(line.clone());
            let trimmed = line.trim_start().trim_start_matches('@');
            if let Some(rest) = trimmed.strip_prefix("just ") {
                let called = rest.split_whitespace().next().unwrap_or("").to_string();
                if !called.is_empty() && !called.starts_with('-') {
                    queue.push(called);
                }
            }
        }
    }
    lines
}

#[test]
fn bench_recipes_are_not_reachable_from_ci() {
    let recipes = parse_recipes(&justfile_text());
    assert!(recipes.contains_key("ci"), "justfile must define `ci`");
    let reachable = ci_reachable_lines(&recipes);
    for line in &reachable {
        for name in BENCH_RECIPES {
            assert!(
                !line.contains(name),
                "`ci` must not reach bench recipe `{name}` (found in line: {line:?})"
            );
        }
    }
}

/// Full, untrimmed `name ...params...:` header line per recipe.
fn recipe_headers(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut headers = std::collections::BTreeMap::new();
    for line in text.lines() {
        if is_recipe_header(line) {
            headers.insert(recipe_name(line).to_string(), line.to_string());
        }
    }
    headers
}

#[test]
fn cost_bearing_recipes_require_manifest_and_questions_with_no_default() {
    let headers = recipe_headers(&justfile_text());
    for name in COST_BEARING_DATASET_RECIPES {
        let header = headers
            .get(*name)
            .unwrap_or_else(|| panic!("missing bench recipe `{name}`"));
        let params: Vec<&str> = if QUESTION_SET_RECIPES.contains(name) {
            vec!["manifest", "questions"]
        } else {
            vec!["manifest"]
        };
        for param in params {
            let bare = format!(" {param} ");
            let bare_at_end = format!(" {param}:");
            assert!(
                header.contains(&bare) || header.contains(&bare_at_end),
                "recipe `{name}` must take a required `{param}` parameter: {header:?}"
            );
            let default_single = format!("{param}='");
            let default_double = format!("{param}=\"");
            assert!(
                !header.contains(&default_single) && !header.contains(&default_double),
                "recipe `{name}`'s `{param}` parameter must not have a default value: {header:?}"
            );
        }
    }
}
