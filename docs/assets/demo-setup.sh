#!/bin/sh
# Rebuild the isolated demo environment that demo.tape records against.
# Usage: docs/assets/demo-setup.sh && (cd /private/tmp/hallouminate-demo && vhs demo.tape)
set -eu

DEMO=/private/tmp/hallouminate-demo
REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)

mkdir -p "$DEMO/xdg-config/hallouminate"
printf '[storage]\nground_dir = "%s/ground"\n' "$DEMO" > "$DEMO/xdg-config/hallouminate/config.toml"

git init -q "$DEMO/hallouminate" 2>/dev/null || true
hallouminate init-repo hallouminate --path "$DEMO/hallouminate" --force
# Absolute repo path: works around #277 (path = "." defeats the cosmetic prefix-strip).
printf '[[repository]]\nname = "hallouminate"\npath = "%s/hallouminate"\n' "$DEMO" > "$DEMO/hallouminate/.hallouminate/config.toml"
cp "$REPO_ROOT/.hallouminate/wiki"/*.md "$DEMO/hallouminate/.hallouminate/wiki/"

XDG_CONFIG_HOME="$DEMO/xdg-config" HALLOUMINATE_SOCKET="$DEMO/daemon.sock" hallouminate daemon &
sleep 2
cd "$DEMO/hallouminate"
XDG_CONFIG_HOME="$DEMO/xdg-config" HALLOUMINATE_SOCKET="$DEMO/daemon.sock" hallouminate index

cp "$REPO_ROOT/docs/assets/demo.tape" "$DEMO/demo.tape"
echo "ready: cd $DEMO && vhs demo.tape"