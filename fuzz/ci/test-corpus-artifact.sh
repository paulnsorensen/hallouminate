#!/usr/bin/env bash
# Test corpus-artifact.sh fetch offline with a gh stub on PATH.
#
# Usage: test-corpus-artifact.sh [script]
# script defaults to corpus-artifact.sh next to this file.
set -euo pipefail

script=$(realpath "${1:-$(dirname "$0")/corpus-artifact.sh}")
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/bin" "$root/fixtures/inputs" "$root/tmp"

# The stub answers the four gh api calls that corpus-artifact.sh makes.
cat >"$root/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
fixtures=${STUB_FIXTURES:?}
[[ $1 == api ]] || exit 64
shift
[[ $1 != --paginate ]] || shift
case $1 in
  repos/owner/repo) echo main ;;
  repos/owner/repo/actions/artifacts\?name=*) jq -c '.artifacts[]' "$fixtures/artifacts.json" ;;
  repos/owner/repo/actions/runs/*) cat "$fixtures/run-${1##*/}.json" ;;
  repos/owner/repo/actions/artifacts/*/zip)
    [[ -z ${STUB_ZIP_FAIL:-} ]] || exit 1
    cat "$fixtures/corpus.zip" ;;
  *) echo "gh stub: unexpected call: $*" >&2; exit 64 ;;
esac
EOF
chmod +x "$root/bin/gh"

cat >"$root/fixtures/artifacts.json" <<'EOF'
{"artifacts": [
  {"id": 11, "expired": false, "created_at": "2026-10-01T00:00:00Z",
   "workflow_run": {"id": 101, "head_branch": "main", "repository_id": 1, "head_repository_id": 1}},
  {"id": 22, "expired": false, "created_at": "2026-10-02T00:00:00Z",
   "workflow_run": {"id": 202, "head_branch": "main", "repository_id": 1, "head_repository_id": 1}}
]}
EOF
run_json() {
  printf '{"path": ".github/workflows/fuzz.yml", "head_branch": "main", "event": "%s", "repository": {"id": 1}, "head_repository": {"id": 1}}\n' "$1"
}
run_json schedule >"$root/fixtures/run-101.json"
run_json pull_request >"$root/fixtures/run-202.json"

expected=3
for i in $(seq "$expected"); do
  printf 'input %s' "$i" >"$root/fixtures/inputs/input-$i"
done
(cd "$root/fixtures/inputs" && zip -q "$root/fixtures/corpus.zip" input-*)

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

fetch() {
  PATH="$root/bin:$PATH" TMPDIR="$root/tmp" STUB_FIXTURES="$root/fixtures" \
    GITHUB_REPOSITORY=owner/repo "$script" fetch chunker "$1"
}

# Artifact 22 is newer, but a pull_request run uploaded it.
# fetch must skip it, restore artifact 11, and exit 0.
dest=$root/out/corpus
status=0
log=$(fetch "$dest" 2>&1) || status=$?
echo "$log"
((status == 0)) || fail "fetch exited $status"
count=$(find "$dest" -type f | wc -l)
((count == expected)) || fail "restored $count files, want $expected"
[[ $log == *"from artifact 11"* ]] || fail "did not restore the trusted artifact 11"
[[ -z $(ls -A "$root/tmp") ]] || fail "left temporary files in TMPDIR"
echo "ok: restored $count files and exited 0"

# A failed download exits non-zero and leaves the corpus dir unchanged.
dest=$root/out/failed
mkdir -p "$dest"
status=0
STUB_ZIP_FAIL=1 fetch "$dest" 2>/dev/null || status=$?
((status != 0)) || fail "fetch exited 0 after a download error"
[[ -z $(ls -A "$dest") ]] || fail "download error changed the corpus dir"
[[ -z $(ls -A "$root/tmp") ]] || fail "left temporary files in TMPDIR after an error"
echo "ok: download error exited $status and left the corpus dir unchanged"
