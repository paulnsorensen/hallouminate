//! Cross-manifest drift test for the plugin pack (issue #107).
//!
//! The plugin ships manifests for two harnesses (Claude Code under
//! `.claude-plugin/`, Codex under `.codex-plugin/`) plus a declarative MCP
//! registration (`.mcp.json`) and two marketplace files. These must agree
//! with each other and with the crate version in `Cargo.toml`, or installs
//! silently drift (the pack shipped 0.1.0 while the crate was at 0.1.3).

use std::path::{Path, PathBuf};

use hallouminate::domain::corpus;
use hallouminate::domain::corpus::index_md::{INDEX_END_MARKER, INDEX_START_MARKER};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json(relative: &str) -> serde_json::Value {
    let path = repo_root().join(relative);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn str_at<'a>(value: &'a serde_json::Value, pointer: &str, file: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{file}: missing string at {pointer}"))
}

#[test]
fn plugin_manifests_share_the_crate_name() {
    let claude = read_json("plugins/hallouminate/.claude-plugin/plugin.json");
    let codex = read_json("plugins/hallouminate/.codex-plugin/plugin.json");
    assert_eq!(
        str_at(&claude, "/name", "claude plugin.json"),
        "hallouminate"
    );
    assert_eq!(str_at(&codex, "/name", "codex plugin.json"), "hallouminate");
}

#[test]
fn plugin_versions_match_the_crate_version() {
    let crate_version = env!("CARGO_PKG_VERSION");
    let claude = read_json("plugins/hallouminate/.claude-plugin/plugin.json");
    let codex = read_json("plugins/hallouminate/.codex-plugin/plugin.json");
    assert_eq!(
        str_at(&claude, "/version", "claude plugin.json"),
        crate_version,
        "claude plugin.json version drifted from Cargo.toml"
    );
    assert_eq!(
        str_at(&codex, "/version", "codex plugin.json"),
        crate_version,
        "codex plugin.json version drifted from Cargo.toml"
    );
}

#[test]
fn claude_marketplace_resolves_to_the_plugin_directory() {
    let marketplace = read_json(".claude-plugin/marketplace.json");
    let root = str_at(&marketplace, "/metadata/pluginRoot", "marketplace.json");
    let entry = marketplace
        .pointer("/plugins")
        .and_then(|p| p.as_array())
        .and_then(|plugins| {
            plugins
                .iter()
                .find(|p| p.pointer("/name").and_then(|n| n.as_str()) == Some("hallouminate"))
        })
        .expect("marketplace.json: no plugin entry named hallouminate");
    let source = str_at(entry, "/source", "marketplace.json");
    assert_eq!(source, "./hallouminate");
    let resolved = repo_root().join(root).join(source);
    assert_is_plugin_dir(&resolved);
}

#[test]
fn codex_marketplace_resolves_to_the_plugin_directory() {
    let marketplace = read_json(".agents/plugins/marketplace.json");
    let path = str_at(
        &marketplace,
        "/plugins/0/source/path",
        ".agents/plugins/marketplace.json",
    );
    assert_eq!(path, "./plugins/hallouminate");
    assert_eq!(
        str_at(
            &marketplace,
            "/plugins/0/source/source",
            ".agents/plugins/marketplace.json",
        ),
        "local",
        "codex marketplace must keep a local path source"
    );
    assert_is_plugin_dir(&repo_root().join(path));
}

#[test]
fn payload_mcp_json_registers_the_path_binary_serve_command() {
    let mcp = read_json("plugins/hallouminate/.mcp.json");
    assert_eq!(
        str_at(&mcp, "/mcpServers/hallouminate/command", ".mcp.json"),
        "hallouminate",
        ".mcp.json must invoke the PATH binary, not an absolute path"
    );
    assert_eq!(
        mcp.pointer("/mcpServers/hallouminate/args"),
        Some(&serde_json::json!(["serve"])),
        ".mcp.json args must be exactly [\"serve\"]"
    );
}

fn assert_is_plugin_dir(resolved: &Path) {
    assert!(
        resolved.join(".claude-plugin/plugin.json").is_file(),
        "{} does not contain .claude-plugin/plugin.json",
        resolved.display()
    );
    assert!(
        resolved.join("skills/install/SKILL.md").is_file(),
        "{} does not contain the install skill",
        resolved.display()
    );
}

#[test]
fn release_workflow_tag_trigger_stays_anchored_at_v() {
    // The double-publish risk: a `skills-v*` or greedy `**` tag trigger would
    // re-fire release-skills.yml on the `skills-v` tag this workflow itself
    // creates, publishing a second release. Only the `v[0-9]+` pattern is safe.
    let path = repo_root().join(".github/workflows/release-skills.yml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

    // serde_yaml 0.9 parses bare `on` as Value::String("on") (YAML 1.2 semantics —
    // only true/false are booleans, not on/off/yes/no).
    let tags = workflow
        .get("on")
        .and_then(|o| o.get("push"))
        .and_then(|p| p.get("tags"))
        .and_then(|t| t.as_sequence())
        .expect("release-skills.yml: on.push.tags must be a sequence");

    // Exactly the anchored crate-tag pattern must be present.
    let tag_strs: Vec<&str> = tags
        .iter()
        .map(|v| v.as_str().expect("tag pattern must be a string"))
        .collect();
    assert_eq!(
        tag_strs,
        ["v[0-9]+.[0-9]+.[0-9]+*"],
        "release-skills.yml on.push.tags must equal exactly [\"v[0-9]+.[0-9]+.[0-9]+*\"]"
    );

    // Every entry must start with `v` (anchored). No `skills-v` or `**` globs.
    for pattern in &tag_strs {
        assert!(
            pattern.starts_with('v'),
            "tag pattern {pattern:?} must start with 'v' to stay anchored"
        );
        assert!(
            !pattern.starts_with("skills-v"),
            "tag pattern {pattern:?} must not start with 'skills-v' (re-trigger risk)"
        );
        assert!(
            !pattern.starts_with("**"),
            "tag pattern {pattern:?} must not be a greedy ** glob"
        );
    }
}

#[test]
fn every_pack_skill_opens_with_description_frontmatter() {
    // Local mirror of the "Validate skill frontmatter" CI step in
    // release-skills.yml, so a broken SKILL.md fails `cargo test` before push.
    let skills_dir = repo_root().join("plugins/hallouminate/skills");
    let mut checked = Vec::new();
    for entry in std::fs::read_dir(&skills_dir).expect("read skills dir") {
        let entry = entry.expect("dir entry").path();
        let skill = entry.join("SKILL.md");
        if !skill.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&skill)
            .unwrap_or_else(|e| panic!("read {}: {e}", skill.display()));
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("---"),
            "{}: SKILL.md must open with a --- frontmatter block",
            skill.display()
        );
        let frontmatter: Vec<&str> = lines.take_while(|line| *line != "---").collect();
        assert!(
            frontmatter.iter().any(|l| l.starts_with("description:")),
            "{}: frontmatter is missing a description:",
            skill.display()
        );
        checked.push(
            entry
                .file_name()
                .expect("skill dir name")
                .to_string_lossy()
                .into_owned(),
        );
    }
    for required in [
        "install",
        "wiki-ingest",
        "wiki-init",
        "wiki-query",
        "wiki-roadmap",
    ] {
        assert!(
            checked.contains(&required.to_string()),
            "pack skill {required} is missing its SKILL.md (found: {checked:?})"
        );
    }
}

fn read_pack_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// First non-blank line after the closing `---` fence — the H1-first rule's
/// subject for both hallouminate's chunker and milknado's `extract_title`.
fn first_body_line(text: &str) -> &str {
    let body = text
        .splitn(3, "---\n")
        .nth(2)
        .expect("template must open with a ----fenced frontmatter block");
    body.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// Content between the two `---` fences.
fn frontmatter_block(text: &str) -> &str {
    text.split("---\n")
        .nth(1)
        .expect("template must open with a ----fenced frontmatter block")
}

/// True when the frontmatter block (not the body) carries a `key:` line.
fn frontmatter_has_key(text: &str, key: &str) -> bool {
    frontmatter_block(text).lines().any(|l| l.starts_with(key))
}

#[test]
fn roadmap_templates_match_the_milknado_import_contract() {
    // Pins the shape milknado's `roadmap import` consumes (milknado
    // src/milknado/domains/wiki/: importer.py + _serialize.py): a leading
    // frontmatter block (stamping `created` via set_frontmatter_field errors
    // without one), a `created:` key that keys the deterministic wiki_ref,
    // and an H1-first title that becomes the node description. Harvest
    // markers stay out — `milknado roadmap export` appends and owns them.
    for relative in [
        "plugins/hallouminate/templates/roadmap/index.md",
        "plugins/hallouminate/templates/roadmap/goal.md",
    ] {
        let text = read_pack_file(relative);
        assert!(
            text.starts_with("---\n"),
            "{relative}: must open with a frontmatter block"
        );
        assert!(
            frontmatter_has_key(&text, "created:"),
            "{relative}: frontmatter must carry created:"
        );
        assert!(
            first_body_line(&text).starts_with("# "),
            "{relative}: first body line must be the H1 title"
        );
        assert!(
            !text.contains("milknado:harvest"),
            "{relative}: harvest markers are milknado-owned"
        );
    }
    let goal = read_pack_file("plugins/hallouminate/templates/roadmap/goal.md");
    assert!(
        frontmatter_has_key(&goal, "prereqs:"),
        "goal template frontmatter must carry prereqs:"
    );
    for required in ["\n## Intent", "\n## Acceptance"] {
        assert!(
            goal.contains(required),
            "goal template must carry {required:?}"
        );
    }
    assert!(
        !goal.contains("## Outcome"),
        "goal template must not pre-add the milknado-owned Outcome section"
    );
}

#[test]
fn wiki_entry_template_follows_the_authoring_conventions() {
    // The conventions SERVER_INSTRUCTIONS pins (src/adapters/mcp/tools.rs):
    // optional frontmatter with the recognized lifecycle keys, H1 as the
    // first body line, footnote citations.
    let text = read_pack_file("plugins/hallouminate/templates/wiki-entry.md");
    assert!(text.starts_with("---\n"), "must demo the frontmatter block");
    for key in ["status:", "last_verified:", "sources:"] {
        assert!(
            frontmatter_has_key(&text, key),
            "frontmatter must demo {key}"
        );
    }
    assert!(
        first_body_line(&text).starts_with("# "),
        "first body line must be the H1 title"
    );
    assert!(
        text.contains("[^1]"),
        "must demo the footnote citation convention"
    );
}

#[test]
fn wiki_roadmap_skill_points_at_the_shipped_templates() {
    let skill = read_pack_file("plugins/hallouminate/skills/wiki-roadmap/SKILL.md");
    assert!(
        skill.contains("templates/roadmap/"),
        "skill must reference the roadmap templates"
    );
    assert!(
        skill.contains("milknado roadmap import"),
        "skill must name the import command the format serves"
    );
    // The skill inlines index/goal skeletons for harnesses that copy only
    // SKILL.md; pin the same contract tokens the template files carry so
    // skeleton drift fails this gate too.
    for token in ["created:", "prereqs:", "## Intent", "## Acceptance"] {
        assert!(
            skill.contains(token),
            "skill's inline skeletons must carry {token:?}"
        );
    }
}

#[test]
fn template_frontmatter_parses_with_the_crates_own_parser() {
    // The placeholders (`<YYYY-MM-DD>`, inline `#` comments) must still form a
    // valid YAML mapping: split_frontmatter is fail-soft, and a malformed
    // block stays in the body — polluting chunks at index time and defeating
    // milknado's `created` stamping (set_frontmatter_field needs a parseable
    // leading block).
    for relative in [
        "plugins/hallouminate/templates/wiki-entry.md",
        "plugins/hallouminate/templates/roadmap/index.md",
        "plugins/hallouminate/templates/roadmap/goal.md",
    ] {
        let text = read_pack_file(relative);
        let (frontmatter, body, _) = corpus::split_frontmatter(&text);
        assert!(
            frontmatter.is_some(),
            "{relative}: frontmatter must parse as a YAML mapping"
        );
        assert!(
            body.trim_start().starts_with("# "),
            "{relative}: the parser-stripped body must open with the H1"
        );
    }
    let entry = read_pack_file("plugins/hallouminate/templates/wiki-entry.md");
    let (frontmatter, _, _) = corpus::split_frontmatter(&entry);
    assert_eq!(
        frontmatter.expect("entry frontmatter").status,
        Some(corpus::LifecycleStatus::Draft),
        "wiki-entry must demo a recognized lifecycle status"
    );
}

#[test]
fn roadmap_index_template_carries_the_daemon_index_markers() {
    // With the marker pair present, the daemon maintains the goal link list
    // between them; if the markers drift from the constants the daemon
    // matches, the template silently opts out of auto link-list maintenance.
    let text = read_pack_file("plugins/hallouminate/templates/roadmap/index.md");
    assert!(
        text.contains(INDEX_START_MARKER) && text.contains(INDEX_END_MARKER),
        "roadmap index template must carry the daemon's exact marker pair"
    );
}

#[test]
fn release_workflow_guards_against_double_publish() {
    // Two safety invariants in release-skills.yml prevent a second `gh release
    // create` from running on a tag whose release already exists, and prevent a
    // version/tag mismatch from silently shipping the wrong pack.
    let path = repo_root().join(".github/workflows/release-skills.yml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

    let steps = workflow
        .get("jobs")
        .and_then(|j| j.get("release-skills"))
        .and_then(|j| j.get("steps"))
        .and_then(|s| s.as_sequence())
        .expect("release-skills.yml: jobs.release-skills.steps must be a sequence");

    let release_run = steps
        .iter()
        .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("Create GitHub Release"))
        .and_then(|s| s.get("run"))
        .and_then(|r| r.as_str())
        .expect("release-skills.yml: 'Create GitHub Release' step must have a run field");

    // The existing-release branch: check before creating, refresh assets with
    // --clobber instead of double-publishing.
    assert!(
        release_run.contains("gh release view"),
        "'Create GitHub Release' must check for an existing release with `gh release view`"
    );
    assert!(
        release_run.contains("--clobber"),
        "'Create GitHub Release' must refresh existing assets with --clobber (not double-publish)"
    );

    // The version/tag mismatch guard lives in the 'Read pack version' step.
    let ver_run = steps
        .iter()
        .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("Read pack version"))
        .and_then(|s| s.get("run"))
        .and_then(|r| r.as_str())
        .expect("release-skills.yml: 'Read pack version' step must have a run field");

    // The comparison that aborts when the pushed tag doesn't match plugin.json.
    assert!(
        ver_run.contains("\"$GITHUB_REF_NAME\" != \"v$v\""),
        "'Read pack version' must guard against tag/plugin-version mismatch via GITHUB_REF_NAME != v$v"
    );
}
