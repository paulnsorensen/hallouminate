#!/usr/bin/env bash
# Open or update one issue per fuzz target whose leg failed in this run
# attempt. The issue title is "Fuzz failure: <target>". An open issue with
# that exact title gets a comment with the run URL; else a new issue opens.
#
# Usage: report-failures.sh
# Reads GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT, and
# GITHUB_SERVER_URL. Needs gh with actions: read and issues: write.
# DRY_RUN=1 prints the issue commands and writes nothing.
set -euo pipefail

repo=${GITHUB_REPOSITORY:?set GITHUB_REPOSITORY to owner/name}
run_id=${GITHUB_RUN_ID:?set GITHUB_RUN_ID}
attempt=${GITHUB_RUN_ATTEMPT:-1}
run_url="${GITHUB_SERVER_URL:-https://github.com}/$repo/actions/runs/$run_id/attempts/$attempt"

run() {
  if [[ ${DRY_RUN:-} == 1 ]]; then
    printf 'dry run:'
    printf ' %q' "$@"
    echo
  else
    "$@"
  fi
}

# Matrix legs are named "fuzz (<target>)" in fuzz.yml.
targets=$(
  gh api --paginate "repos/$repo/actions/runs/$run_id/attempts/$attempt/jobs?per_page=100" --jq '.jobs[]' |
    jq -r 'select(.conclusion == "failure" or .conclusion == "timed_out")
      | .name | capture("^fuzz \\((?<t>[A-Za-z0-9_]+)\\)$").t'
)
if [[ -z $targets ]]; then
  echo "::warning::no failed fuzz leg found in run $run_id attempt $attempt"
  exit 0
fi

while read -r target; do
  title="Fuzz failure: $target"
  number=$(
    gh issue list --repo "$repo" --state open --search "\"$title\" in:title" --json number,title |
      jq -r --arg title "$title" 'map(select(.title == $title)) | .[0].number // empty'
  )
  if [[ -n $number ]]; then
    run gh issue comment "$number" --repo "$repo" --body "Failed again: $run_url"
  else
    body=$(
      cat <<EOF
The fuzz leg for \`$target\` failed: $run_url

Download the \`fuzz-crash-$target\` artifact from the run and follow
"Reproduce a CI crash" in \`fuzz/README.md\`. Later failures add a comment here.
EOF
    )
    run gh issue create --repo "$repo" --title "$title" --body "$body"
  fi
done <<<"$targets"
