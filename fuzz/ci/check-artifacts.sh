#!/usr/bin/env bash
# Fail when a libFuzzer artifact directory holds a crash, timeout, OOM, or
# leak input. Fork mode (cargo fuzz run --jobs N) ignores timeouts and OOMs
# by default, so the exit status alone does not report them.
# slow-unit-* inputs are not failures.
#
# Usage: check-artifacts.sh <dir>
set -euo pipefail

dir=${1:?usage: check-artifacts.sh <dir>}
[[ -d $dir ]] || exit 0

shopt -s nullglob
found=("$dir"/{crash,timeout,oom,leak}-*)
((${#found[@]} > 0)) || exit 0

printf '::error::fuzz artifact %s\n' "${found[@]}"
exit 1
