#!/usr/bin/env bash
# Inspect tracked path metadata only; never read or print environment values.
set -euo pipefail

forbidden="$(git ls-files 'env/dec/*.env' '.age/**' '.sops/**' '*.agekey' '*.age-key' 'age-key*.txt' 'age-keys*.txt')"
if [[ -n "$forbidden" ]]; then
  echo 'Tracked plaintext environment or age identity material is forbidden.' >&2
  exit 1
fi

# Quoted Git paths fail closed; unusual names cannot spoof one allowed line.
encrypted="$(git -c core.quotePath=true ls-files 'env/enc/**')"
while IFS= read -r path; do
  case "$path" in
    ''|env/enc/.gitkeep|env/enc/*.env.enc) ;;
    *) echo 'env/enc contains a tracked file outside *.env.enc.' >&2; exit 1 ;;
  esac
done <<< "$encrypted"
