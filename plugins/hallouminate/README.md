# Hallouminate skill pack

A cross-harness plugin that installs
[hallouminate](https://github.com/paulnsorensen/hallouminate) and bootstraps
your first LLM-authored, per-repo wiki.

## Install

**Claude Code:**

```text
/plugin marketplace add paulnsorensen/hallouminate
/plugin install hallouminate@hallouminate
```

Then run the install workflow:

```text
/hallouminate:install
```

(Or just ask Claude to "install hallouminate" — the `install` skill is
model-invocable too.)

**Codex:**

```text
codex plugin marketplace add paulnsorensen/hallouminate
```

Then restart Codex and install from the `/plugins` directory.

**opencode:** copy the skills and register the MCP server — see the
[install matrix](../../README.md#per-harness-setup) in the root README.

## What `/install` does

The `install` skill walks you from zero to a working wiki:

1. **Platform check** — prebuilt targets, with loud fallbacks for Intel macOS
   (source build) and Windows (unsupported, #48).
2. **Install** — prebuilt-binary cascade: `hallouminate-installer.sh` →
   `cargo binstall` → `cargo install --locked` source fallback. No Rust
   toolchain or `protoc` needed on supported targets.
3. **MCP** — the bundled `.mcp.json` registers `hallouminate serve`
   declaratively; `claude mcp add --scope user` survives as the user-scope
   fallback.
4. **Config** — `hallouminate config init` + `validate`.
5. **Socratic discovery** — asks where the wiki should live, what to capture
   first, who reads it, and how to handle git.
6. **Seed** — `hallouminate init-repo <name>` writes the repo-layer
   `.hallouminate/config.toml` tenant plus the wiki skeleton, and narrates the
   first daemon spawn.
7. **Author** — writes a first, grounded wiki page via `add_markdown`.
8. **Index & verify** — indexes and proves it with a `ground` query.
9. **Commit** — branches and commits the scaffolding with git.

## Layout

```text
plugins/hallouminate/
├── .claude-plugin/plugin.json     # Claude Code plugin manifest
├── .codex-plugin/plugin.json      # Codex plugin manifest
├── .mcp.json                      # declarative MCP registration (hallouminate serve)
├── skills/install/SKILL.md        # the /install workflow
├── skills/wiki-init/SKILL.md      # Socratic wiki bootstrap
├── skills/wiki-ingest/SKILL.md    # fold source docs into the wiki
├── skills/wiki-query/SKILL.md     # answer questions from the wiki
└── README.md
```

The Claude Code marketplace manifest lives at the repository root in
`.claude-plugin/marketplace.json`; the Codex one in
`.agents/plugins/marketplace.json`. `tests/plugin_manifests.rs` pins both
manifests (and this pack's version) to the crate version in `Cargo.toml`.
Releases are published by the `release-skills` GitHub Actions workflow on
every `v*` release tag.
