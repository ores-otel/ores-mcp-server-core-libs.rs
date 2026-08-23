#!/bin/sh
# ores-lint shared configuration. Sourced by lint.sh, js.sh and rust.sh.
# Every value can be overridden from the environment.

# Maximum number of concrete example locations shown for any one rule.
: "${ORES_LINT_MAX_EXAMPLES:=5}"

# Warn-only by default: lint.sh exits 0 no matter what it finds.
# Flip to 1 (per repo, or in CI) once a repo's debt is paid down.
: "${ORES_LINT_STRICT:=0}"

: "${ORES_LINT_SKIP_RUST:=0}"
: "${ORES_LINT_SKIP_JS:=0}"

# Include tests/benches/examples in the Rust pass. Off by default so the
# pre-publish signal is about shipped code.
: "${ORES_LINT_RUST_ALL_TARGETS:=0}"

# The exact clippy diagnostic text for `clippy::implicit_return`. selftest.sh
# verifies this still matches, so a future clippy rewording surfaces as a test
# failure rather than as a silently empty report.
: "${ORES_LINT_IMPLICIT_RETURN_MSG:=missing \`return\` statement}"

export ORES_LINT_MAX_EXAMPLES ORES_LINT_STRICT ORES_LINT_SKIP_RUST ORES_LINT_SKIP_JS
export ORES_LINT_RUST_ALL_TARGETS ORES_LINT_IMPLICIT_RETURN_MSG

# Repo-local overrides, never overwritten by the rollout script.
DIR_CFG=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
[ -f "$DIR_CFG/local.sh" ] && . "$DIR_CFG/local.sh"
