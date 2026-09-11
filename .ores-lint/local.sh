# Repo-local ores-lint overrides. Never overwritten by the rollout script.
#
# This crate is published to crates.io, so the pre-publish signal should cover
# tests and examples as well as shipped code.
ORES_LINT_RUST_ALL_TARGETS=1

# Deny-by-default extras beyond the fleet baseline, appropriate for a library
# whose panics become someone else's production incident.
ORES_LINT_RUST_EXTRA="-W clippy::indexing_slicing -W clippy::string_slice -W clippy::unreachable"
