#!/usr/bin/env bash
# Find and download the newest trusted fuzz corpus artifact for one target.
#
# Usage:
#   corpus-artifact.sh lookup <target>        print the artifact ID, or "none"
#   corpus-artifact.sh fetch <target> <dir>   add the artifact files to <dir>
#
# A trusted artifact is not expired and comes from a schedule or
# workflow_dispatch run of fuzz.yml on the default branch of this repository.
# This rejects corpus files that a pull request run uploads.
# A missing artifact is not an error. An API or download error exits non-zero
# and leaves <dir> unchanged.
#
# Needs gh with a token that has actions: read, and GITHUB_REPOSITORY.
# DEFAULT_BRANCH overrides the default branch lookup.
set -euo pipefail

readonly workflow_path=.github/workflows/fuzz.yml

die() {
  echo "corpus-artifact: $*" >&2
  exit 2
}

repo=${GITHUB_REPOSITORY:?set GITHUB_REPOSITORY to owner/name}

# Succeeds when the run that uploaded the artifact is trusted.
trusted_run() {
  local run_id=$1 branch=$2 run
  run=$(gh api "repos/$repo/actions/runs/$run_id") || die "cannot read run $run_id"
  jq -e --arg path "$workflow_path" --arg branch "$branch" '
    .path == $path
    and .head_branch == $branch
    and (.event == "schedule" or .event == "workflow_dispatch")
    and .repository.id != null
    and .head_repository.id == .repository.id' <<<"$run" >/dev/null
}

lookup() {
  local target=$1 branch candidates id run_id
  branch=${DEFAULT_BRANCH:-$(gh api "repos/$repo" --jq .default_branch)}
  candidates=$(
    gh api --paginate "repos/$repo/actions/artifacts?name=fuzz-corpus-$target&per_page=100" --jq '.artifacts[]' |
      jq -rs --arg branch "$branch" '
        map(select(.expired == false
          and .workflow_run.id != null
          and .workflow_run.head_branch == $branch
          and .workflow_run.repository_id != null
          and .workflow_run.head_repository_id == .workflow_run.repository_id))
        | sort_by(.created_at) | reverse | .[]
        | "\(.id) \(.workflow_run.id)"'
  ) || die "cannot list artifacts for $target"
  while read -r id run_id; do
    [[ -n $id ]] || continue
    if trusted_run "$run_id" "$branch"; then
      echo "$id"
      return
    fi
  done <<<"$candidates"
  echo none
}

fetch() {
  local target=$1 dir=$2 id tmp count
  id=$(lookup "$target")
  if [[ $id == none ]]; then
    echo "corpus-artifact: no corpus artifact for $target; start from seeds" >&2
    return
  fi
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  gh api "repos/$repo/actions/artifacts/$id/zip" >"$tmp/corpus.zip" || die "cannot download artifact $id"
  unzip -q "$tmp/corpus.zip" -d "$tmp/files" || die "cannot extract artifact $id"
  count=$(find "$tmp/files" -type f | wc -l)
  mkdir -p "$dir"
  find "$tmp/files" -type f -exec mv -f -t "$dir" {} +
  echo "corpus-artifact: restored $count inputs for $target from artifact $id" >&2
}

case ${1:-} in
  lookup) (($# == 2)) || die "usage: $0 lookup <target>"; lookup "$2" ;;
  fetch) (($# == 3)) || die "usage: $0 fetch <target> <dir>"; fetch "$2" "$3" ;;
  *) die "usage: $0 lookup <target> | fetch <target> <dir>" ;;
esac
