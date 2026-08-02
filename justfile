set unstable
set lists
# hallouminate dev commands — run `just` to list recipes.
#
# rustup honors rust-toolchain.toml (the crate's MSRV). protoc must be
# installed locally for the lancedb build script:
#   macOS:  brew install protobuf
#   Debian: sudo apt-get install -y protobuf-compiler

default:
    @just --list

# Canonical local gate. No args runs fmt, clippy, build, and tests; targeted
# Cargo commands use the same cross-worktree verification lease.
verify *args:
    python3 scripts/verify.py {{quote(args)}}

# Compatibility routes retained for contributors and release automation.
ci:
    just verify

llm:
    just verify --fix

# Measure the production fusion baseline against the Jina-v1 diagnostic arm.
eval-measure:
    HALLOUMINATE_EVAL_OUTPUT=.context/issue-288-eval-results.json just verify cargo test -p hallouminate --test eval_ground_recall eval_ground_recall_measure -- --ignored --nocapture --exact

# Enforce the committed production fusion baseline without a crossencoder.
eval:
    just verify cargo test -p hallouminate --test eval_ground_recall eval_ground_recall_enforce -- --ignored --nocapture --exact

# Validate the manifest and question set together. Cheap and safe — this is
# the one a human runs locally before any bench-* recipe below. Defaults
# target the committed example fixtures, so `just bench-validate` passes
# today; point at the real dataset once it exists and is frozen:
# `just bench-validate eval/agent-bench/manifest.toml eval/agent-bench/questions.json`.
# Do not create empty manifest.toml/questions.json stubs to make this pass —
# an empty file would validate clean and defeat the freeze discipline the
# whole benchmark rests on.
bench-validate manifest='eval/agent-bench/manifest.example.toml' questions='eval/agent-bench/questions.example.json':
    just verify cargo run -q -p agent-bench --bin bench-validate-manifest -- --manifest {{manifest}}
    just verify cargo run -q -p agent-bench --bin bench-validate-questions -- --questions {{questions}} --manifest {{manifest}}

# Author one subject repo's wiki under a token budget. Cost-bearing: spends
# real model tokens. Validates the committed example manifest/questions
# first so authoring never runs against an unfrozen or drifted dataset.
bench-author repo budget:
    @echo "bench-author: cost-bearing run — spends real model tokens"
    just verify cargo run -q -p agent-bench --bin bench-validate-manifest -- --manifest eval/agent-bench/manifest.example.toml
    just verify cargo run -q -p agent-bench --bin bench-validate-questions -- --questions eval/agent-bench/questions.example.json --manifest eval/agent-bench/manifest.example.toml
    just verify cargo run -q -p agent-bench --bin bench-author -- --manifest eval/agent-bench/manifest.example.toml --repo {{repo}} --budget-tokens {{budget}} --out-dir eval/agent-bench/wikis/{{repo}}

# Paired session sweep across both arms. Cost-bearing: spends real model
# tokens. Validates the committed example manifest/questions first so no
# measured run starts against an unfrozen or drifted dataset.
bench-run arm='both' runs='10':
    @echo "bench-run: cost-bearing run — spends real model tokens"
    just verify cargo run -q -p agent-bench --bin bench-validate-manifest -- --manifest eval/agent-bench/manifest.example.toml
    just verify cargo run -q -p agent-bench --bin bench-validate-questions -- --questions eval/agent-bench/questions.example.json --manifest eval/agent-bench/manifest.example.toml
    just verify cargo run -q -p agent-bench --bin bench-run -- --manifest eval/agent-bench/manifest.example.toml --questions eval/agent-bench/questions.example.json --arm {{arm}} --runs {{runs}} --out-dir eval/agent-bench/results

# Grade sessions against the question set. Cost-bearing: spends real judge
# tokens. Validates the committed example manifest/questions first so
# grading never runs against an unfrozen or drifted dataset.
bench-judge:
    @echo "bench-judge: cost-bearing run — spends real judge tokens"
    just verify cargo run -q -p agent-bench --bin bench-validate-manifest -- --manifest eval/agent-bench/manifest.example.toml
    just verify cargo run -q -p agent-bench --bin bench-validate-questions -- --questions eval/agent-bench/questions.example.json --manifest eval/agent-bench/manifest.example.toml
    just verify cargo run -q -p agent-bench --bin bench-judge -- --sessions eval/agent-bench/results/sessions.jsonl --questions eval/agent-bench/questions.example.json --out eval/agent-bench/results/grades.jsonl

# Calibrate the automated judge against a human-labelled grades.jsonl.
# Cost-bearing: spends real judge tokens. Validates the committed example
# manifest/questions first so calibration never runs against an unfrozen or
# drifted dataset.
bench-judge-calibrate human_grades:
    @echo "bench-judge-calibrate: cost-bearing run — spends real judge tokens"
    just verify cargo run -q -p agent-bench --bin bench-validate-manifest -- --manifest eval/agent-bench/manifest.example.toml
    just verify cargo run -q -p agent-bench --bin bench-validate-questions -- --questions eval/agent-bench/questions.example.json --manifest eval/agent-bench/manifest.example.toml
    just verify cargo run -q -p agent-bench --bin bench-judge -- --sessions eval/agent-bench/results/sessions.jsonl --questions eval/agent-bench/questions.example.json --out eval/agent-bench/results/grades.jsonl --calibrate {{human_grades}}

# Aggregate sessions and grades into report.json/report.md. Not
# cost-bearing: reads already-recorded records, spends no model tokens.
bench-report:
    just verify cargo run -q -p agent-bench --bin bench-report -- --sessions eval/agent-bench/results/sessions.jsonl --grades eval/agent-bench/results/grades.jsonl --questions eval/agent-bench/questions.example.json --out-dir eval/agent-bench/results --seed 42

# Prepare a new release bump PR: crate version, lockfile, and plugin manifests.
prepare-release version:
    #!/usr/bin/env bash
    set -euo pipefail

    version='{{version}}'
    if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
        echo "version must be SemVer without a leading v: $version" >&2
        exit 2
    fi

    git diff --quiet HEAD || { echo "working tree dirty — commit first" >&2; exit 1; }
    git diff --cached --quiet || { echo "index dirty — commit first" >&2; exit 1; }
    git fetch origin main --tags

    if [ "$(git branch --show-current)" != main ]; then
        echo "prepare-release must start from main" >&2
        exit 1
    fi
    if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
        echo "main is not up to date with origin/main" >&2
        exit 1
    fi

    branch="release/v$version"
    if git show-ref --verify --quiet "refs/heads/$branch" || git ls-remote --exit-code --heads origin "$branch" >/dev/null 2>&1; then
        echo "release branch already exists: $branch" >&2
        exit 1
    fi
    if git show-ref --verify --quiet "refs/tags/v$version" || git ls-remote --exit-code --tags origin "refs/tags/v$version" >/dev/null 2>&1; then
        echo "release tag already exists: v$version" >&2
        exit 1
    fi
    if gh release view "v$version" >/dev/null 2>&1; then
        echo "GitHub release already exists: v$version" >&2
        exit 1
    fi

    git switch -c "$branch"
    python3 - "$version" <<'PY'
    import json
    import re
    import sys
    from pathlib import Path

    version = sys.argv[1]

    cargo = Path("Cargo.toml")
    lines = cargo.read_text().splitlines()
    in_workspace_package = False
    version_set = False
    for index, line in enumerate(lines):
        if line == "[workspace.package]":
            in_workspace_package = True
            continue
        if in_workspace_package and line.startswith("["):
            in_workspace_package = False
        if in_workspace_package and not version_set and line.startswith("version = "):
            lines[index] = f'version = "{version}"'
            version_set = True
        if re.match(r"hallouminate-(domain|adapters|config|daemon) = ", line):
            lines[index] = re.sub(r'version = "[^"]*"', f'version = "{version}"', line)
    if not version_set:
        raise SystemExit("Cargo.toml [workspace.package].version not found")
    cargo.write_text("\n".join(lines) + "\n")

    for manifest in [
        Path("plugins/hallouminate/.claude-plugin/plugin.json"),
        Path("plugins/hallouminate/.codex-plugin/plugin.json"),
        Path("plugins/hallouminate/plugin.json"),
        Path("plugins/hallouminate/.cursor-plugin/plugin.json"),
        Path("plugins/hallouminate/gemini-extension.json"),
        Path("npm/package.json"),
    ]:
        data = json.loads(manifest.read_text())
        data["version"] = version
        manifest.write_text(json.dumps(data, indent=2) + "\n")
    PY

    cargo update -p hallouminate --precise "$version"
    just ci

    git add Cargo.toml Cargo.lock npm/package.json \
        plugins/hallouminate/.claude-plugin/plugin.json \
        plugins/hallouminate/.codex-plugin/plugin.json \
        plugins/hallouminate/plugin.json \
        plugins/hallouminate/.cursor-plugin/plugin.json \
        plugins/hallouminate/gemini-extension.json
    git commit -m "chore(release): bump version to $version"
    git push -u origin "$branch"
    gh pr create --base main --head "$branch" --title "chore(release): bump version to $version" --body "Release bump for v$version."

# Release a prepared version from main by pushing v<version>, which triggers dist, crates.io, and skills workflows.
release version:
    #!/usr/bin/env bash
    set -euo pipefail

    version='{{version}}'
    if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
        echo "version must be SemVer without a leading v: $version" >&2
        exit 2
    fi

    git diff --quiet HEAD || { echo "working tree dirty — commit first" >&2; exit 1; }
    git diff --cached --quiet || { echo "index dirty — commit first" >&2; exit 1; }
    git fetch origin main --tags

    if [ "$(git branch --show-current)" != main ]; then
        echo "release must run from main after the release PR is merged" >&2
        exit 1
    fi
    if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
        echo "main is not up to date with origin/main" >&2
        exit 1
    fi

    manifest_version="$(cargo metadata --no-deps --format-version=1 | jq -r '.packages[] | select(.name == "hallouminate") | .version')"
    claude_version="$(jq -r .version plugins/hallouminate/.claude-plugin/plugin.json)"
    codex_version="$(jq -r .version plugins/hallouminate/.codex-plugin/plugin.json)"
    copilot_version="$(jq -r .version plugins/hallouminate/plugin.json)"
    cursor_version="$(jq -r .version plugins/hallouminate/.cursor-plugin/plugin.json)"
    gemini_version="$(jq -r .version plugins/hallouminate/gemini-extension.json)"
    if [ "$manifest_version" != "$version" ] || [ "$claude_version" != "$version" ] || [ "$codex_version" != "$version" ] || [ "$copilot_version" != "$version" ] || [ "$cursor_version" != "$version" ] || [ "$gemini_version" != "$version" ]; then
        echo "version mismatch: Cargo=$manifest_version Claude=$claude_version Codex=$codex_version Copilot=$copilot_version Cursor=$cursor_version Gemini=$gemini_version target=$version" >&2
        exit 1
    fi

    if git show-ref --verify --quiet "refs/tags/v$version" || git ls-remote --exit-code --tags origin "refs/tags/v$version" >/dev/null 2>&1; then
        echo "release tag already exists: v$version" >&2
        exit 1
    fi
    if gh release view "v$version" >/dev/null 2>&1; then
        echo "GitHub release already exists: v$version" >&2
        exit 1
    fi

    if ! gh run list --branch main --commit "$(git rev-parse HEAD)" --workflow CI --json conclusion --jq '.[0].conclusion == "success"' | grep -qx true; then
        echo "latest CI run for HEAD is not green" >&2
        exit 1
    fi

    git tag "v$version"
    git push origin "v$version"

# Move v<version> to HEAD and push, retriggering release.yml + publish-crates.yml + release-skills.yml.
re-tag version:
    @git diff --quiet HEAD || { echo "working tree dirty — commit first"; exit 1; }
    -gh release delete v{{version}} --yes
    -git push origin :refs/tags/v{{version}}
    -git tag -d v{{version}}
    git tag v{{version}}
    git push origin v{{version}}

# Regenerate docs/assets/demo.gif; needs the hallouminate binary and vhs on PATH.
demo:
    docs/assets/demo-setup.sh
