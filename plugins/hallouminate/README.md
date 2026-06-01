# Hallouminate skill pack

A Claude Code plugin that installs [hallouminate](https://github.com/paulnsorensen/hallouminate)
and bootstraps your first LLM-authored, per-repo wiki.

## Install

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

## What `/install` does

The `install` skill walks you from zero to a working wiki:

1. **Preflight** — checks `cargo` and `protoc` (the `lancedb` build needs it).
2. **Install** — `cargo install hallouminate --locked` and verifies the binary.
3. **MCP** — registers `hallouminate serve` with `claude mcp add` so the
   `ground` / `add_markdown` / `read_markdown` / `delete_markdown` tools load.
4. **Config** — `hallouminate config init` + `validate`.
5. **Socratic discovery** — asks where the wiki should live, what to capture
   first, who reads it, and how to handle git.
6. **Scaffold** — writes the repo-layer `.hallouminate/config.toml` tenant.
7. **Author** — writes a first, grounded wiki page via `add_markdown`.
8. **Index & verify** — indexes and proves it with a `ground` query.
9. **Commit** — branches and commits the scaffolding with git.

## Layout

```text
plugins/hallouminate/
├── .claude-plugin/plugin.json     # plugin manifest
├── skills/install/SKILL.md        # the /install workflow
└── README.md
```

The marketplace manifest lives at the repository root in
`.claude-plugin/marketplace.json`. Releases are published by the
`release-skills` GitHub Actions workflow.
