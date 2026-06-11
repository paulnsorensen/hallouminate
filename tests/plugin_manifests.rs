//! Cross-manifest drift test for the plugin pack (issue #107).
//!
//! The plugin ships manifests for two harnesses (Claude Code under
//! `.claude-plugin/`, Codex under `.codex-plugin/`) plus a declarative MCP
//! registration (`.mcp.json`) and two marketplace files. These must agree
//! with each other and with the crate version in `Cargo.toml`, or installs
//! silently drift (the pack shipped 0.1.0 while the crate was at 0.1.3).

use std::path::{Path, PathBuf};

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
