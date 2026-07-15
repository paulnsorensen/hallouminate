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
    import sys
    from pathlib import Path

    version = sys.argv[1]

    cargo = Path("Cargo.toml")
    lines = cargo.read_text().splitlines()
    in_package = False
    changed = False
    for index, line in enumerate(lines):
        if line == "[package]":
            in_package = True
            continue
        if in_package and line.startswith("["):
            break
        if in_package and line.startswith("version = "):
            lines[index] = f'version = "{version}"'
            changed = True
            break
    if not changed:
        raise SystemExit("Cargo.toml [package].version not found")
    cargo.write_text("\n".join(lines) + "\n")

    for manifest in [
        Path("plugins/hallouminate/.claude-plugin/plugin.json"),
        Path("plugins/hallouminate/.codex-plugin/plugin.json"),
        Path("npm/package.json"),
    ]:
        data = json.loads(manifest.read_text())
        data["version"] = version
        manifest.write_text(json.dumps(data, indent=2) + "\n")
    PY

    cargo update -p hallouminate --precise "$version"
    just ci

    git add Cargo.toml Cargo.lock plugins/hallouminate/.claude-plugin/plugin.json plugins/hallouminate/.codex-plugin/plugin.json npm/package.json
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
    if [ "$manifest_version" != "$version" ] || [ "$claude_version" != "$version" ] || [ "$codex_version" != "$version" ]; then
        echo "version mismatch: Cargo=$manifest_version Claude=$claude_version Codex=$codex_version target=$version" >&2
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
