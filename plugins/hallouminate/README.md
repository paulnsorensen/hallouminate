# Hallouminate skill pack

A cross-harness plugin that installs
[hallouminate](https://github.com/paulnsorensen/hallouminate) and bootstraps
your first LLM-authored, per-repo wiki.

## Install

| Harness | Install |
| --- | --- |
| **Claude Code** | `/plugin marketplace add paulnsorensen/hallouminate` → `/plugin install hallouminate@hallouminate` |
| **Codex** | `codex plugin marketplace add paulnsorensen/hallouminate`, restart, then `codex plugin add hallouminate@hallouminate` |
| **Copilot CLI** | `copilot plugin marketplace add paulnsorensen/hallouminate` → `copilot plugin install hallouminate@hallouminate` |
| **OMP** | `/marketplace add paulnsorensen/hallouminate` → `/marketplace install hallouminate@hallouminate` |
| **Cursor** | Teams/Enterprise: import `https://github.com/paulnsorensen/hallouminate` under **Plugins → Team Marketplaces**. Local: clone, copy or symlink `plugins/hallouminate` to `~/.cursor/plugins/local/hallouminate`, then reload/restart Cursor. |
| **Gemini CLI** | `gemini extensions install ./plugins/hallouminate --consent`; for an extracted release archive, use `./hallouminate-skills-<version>/plugins/hallouminate` instead. |
| **opencode** | Copy `skills/` to `~/.config/opencode/skills/` and register `hallouminate serve` in `opencode.json`. |

Claude Code users can then run `/hallouminate:install`. The install skill is
model-invocable on every harness, so asking the agent to "install hallouminate"
starts the same workflow.

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

## Templates

`templates/wiki-entry.md` is the formal shape of a wiki entry — optional
lifecycle frontmatter, H1-first, footnote citations, provenance footer.
`templates/roadmap/` holds the roadmap `index.md` + goal-file pair in exactly
the format [milknado](https://github.com/paulnsorensen/milknado) imports:
author with `/wiki-roadmap`, then `milknado roadmap import <slug>` seeds the
roadmap into an executable graph with no rework.

## Layout

```text
plugins/hallouminate/
├── .claude-plugin/plugin.json     # Claude Code and OMP metadata
├── .codex-plugin/plugin.json      # Codex plugin manifest
├── .cursor-plugin/plugin.json     # Cursor plugin manifest
├── plugin.json                    # Copilot CLI plugin manifest
├── gemini-extension.json          # Gemini CLI extension manifest
├── .mcp.json                      # shared declarative MCP registration
├── skills/                        # install and wiki workflows
├── templates/                     # wiki-entry and roadmap templates
└── README.md
```

Marketplace manifests live at the repository root: Claude Code, Copilot CLI,
and OMP use `.claude-plugin/marketplace.json`; Codex uses
`.agents/plugins/marketplace.json`; Cursor uses
`.cursor-plugin/marketplace.json`. `tests/plugin_manifests.rs` pins every
manifest to the crate version in `Cargo.toml`. The `release-skills` workflow
publishes versioned plugin-pack archives on every `v*` release tag.
