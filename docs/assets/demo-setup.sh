#!/bin/sh
# Regenerate docs/assets/demo.gif in an isolated temporary environment.
set -eu

TMP_ROOT=${TMPDIR:-/tmp}
REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)

command -v hallouminate >/dev/null || { echo "demo-setup: hallouminate not found (cargo install hallouminate)" >&2; exit 1; }
command -v vhs >/dev/null || { echo "demo-setup: vhs not found (brew install vhs)" >&2; exit 1; }

DEMO=$(mktemp -d "${TMP_ROOT%/}/hallouminate-demo.XXXXXX")

mkdir -p "$DEMO/xdg-config/hallouminate"
printf '[storage]\nground_dir = "%s/ground"\n' "$DEMO" > "$DEMO/xdg-config/hallouminate/config.toml"

git init -q "$DEMO/hallouminate"
hallouminate init-repo hallouminate --path "$DEMO/hallouminate" --force
# Absolute repo path: works around #277 (path = "." defeats the cosmetic prefix-strip).
printf '[[repository]]\nname = "hallouminate"\npath = "%s/hallouminate"\n' "$DEMO" > "$DEMO/hallouminate/.hallouminate/config.toml"
cp -R "$REPO_ROOT/.hallouminate/wiki"/. "$DEMO/hallouminate/.hallouminate/wiki/"

XDG_CONFIG_HOME="$DEMO/xdg-config" HALLOUMINATE_SOCKET="$DEMO/daemon.sock" hallouminate daemon &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true; wait "$DAEMON_PID" 2>/dev/null || true; rm -rf "$DEMO"' EXIT
trap 'exit 1' HUP INT TERM

waited=0
until [ -S "$DEMO/daemon.sock" ]; do
    waited=$((waited + 1))
    if [ "$waited" -ge 150 ]; then
        echo "demo-setup: timed out waiting for $DEMO/daemon.sock" >&2
        exit 1
    fi
    sleep 0.2
done

cd "$DEMO/hallouminate"
XDG_CONFIG_HOME="$DEMO/xdg-config" HALLOUMINATE_SOCKET="$DEMO/daemon.sock" hallouminate index

cd "$REPO_ROOT/docs/assets"
XDG_CONFIG_HOME="$DEMO/xdg-config" \
HALLOUMINATE_SOCKET="$DEMO/daemon.sock" \
HALLOUMINATE_DEMO="$DEMO" \
vhs demo.tape

echo "updated: $REPO_ROOT/docs/assets/demo.gif"