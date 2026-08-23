#!/bin/sh
# ores-lint :: JavaScript / TypeScript
#
# Uses whatever eslint the repo already has. Never installs anything, never
# reaches the network, and skips cleanly when eslint is absent.

set -u
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$DIR/config.sh"
ROOT=${1:-.}

[ "${ORES_LINT_SKIP_JS}" = "1" ] && { echo "ores-lint[js]: skipped (ORES_LINT_SKIP_JS=1)"; exit 0; }

# Walk up from the repo root looking for an installed eslint binary, so that
# packages inside a monorepo find the hoisted one.
find_eslint() {
  d=$(CDPATH= cd -- "$1" && pwd)
  while [ -n "$d" ] && [ "$d" != "/" ]; do
    if [ -x "$d/node_modules/.bin/eslint" ]; then printf '%s\n' "$d/node_modules/.bin/eslint"; return 0; fi
    d=$(dirname "$d")
  done
  return 1
}

ESLINT=$(find_eslint "$ROOT") || {
  echo "ores-lint[js]: eslint is not installed here - skipping (run your package manager's install first)"
  exit 0
}

CONFIG=""
for c in eslint.config.mjs eslint.config.js eslint.config.cjs; do
  [ -f "$ROOT/$c" ] && { CONFIG="$ROOT/$c"; break; }
done
[ -z "$CONFIG" ] && { echo "ores-lint[js]: no flat eslint config found - skipping"; exit 0; }

OUT=$(mktemp) || exit 0
RC=0
( cd "$ROOT" && "$ESLINT" . \
    --no-error-on-unmatched-pattern \
    --format "$DIR/eslint/formatter.mjs" ) >"$OUT" 2>&1 || RC=$?

if [ "$RC" -ne 0 ] && ! grep -q 'ores-lint\[js\]' "$OUT"; then
  echo "ores-lint[js]: eslint could not run in $ROOT (exit $RC). First lines:"
  sed -n '1,6p' "$OUT" | sed 's/^/  | /'
  rm -f "$OUT"
  exit 0
fi

cat "$OUT"
rm -f "$OUT"
exit 0
