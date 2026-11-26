#!/usr/bin/env bash
# Runs a command and, when it fails, posts its first error or panic as a GitHub annotation, so
# the cause shows on the run's page: job logs need a signed-in account, annotations do not.
#
#   scripts/ci-annotate.sh "Title" cargo xtask ci integration
set -uo pipefail

title="$1"
shift
log="$(mktemp)"
if "$@" 2>&1 | tee "$log"; then
  exit 0
fi
cause="$(sed 's/\x1b\[[0-9;]*m//g' "$log" | grep -E -m1 -A60 'panicked|^error' | head -80)"
cause="${cause//'%'/'%25'}"
cause="${cause//$'\r'/}"
cause="${cause//$'\n'/'%0A'}"
echo "::error title=${title}::${cause:-The command failed; the job log has the details.}"
exit 1
