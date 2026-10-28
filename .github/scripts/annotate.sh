#!/usr/bin/env bash
# Runs a command; on failure repeats the end of its output as an error annotation,
# which GitHub shows without signing in (raw logs need an account).
set -o pipefail
log=$(mktemp)
"$@" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
if [ "$status" -ne 0 ]; then
  body=$(grep -Ev '^\s*(Compiling|Downloaded|Downloading|Checking|Running|Fresh) ' "$log" | tail -n 70)
  body=${body//'%'/'%25'}
  body=${body//$'\r'/'%0D'}
  body=${body//$'\n'/'%0A'}
  echo "::error title=$1 failed::$body"
fi
exit "$status"
