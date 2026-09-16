# AGENTS.md — ores-otel/ores-mcp-server-core-libs.rs

## Parent / root agent contract

This file is **this repository's** agent contract. The fleet-wide parent lives at:

- GitHub: https://github.com/oresoftware/my-ai/AGENTS.md
- Disk: `~/codes/oresoftware/my-ai/AGENTS.md`
- Installed by `~/codes/oresoftware/my-ai/setup-final.sh` (not `.md`) as symlinks onto:
  - `~/codes/AGENTS.md`
  - `~/codes/.claude/AGENTS.md` and `~/codes/.claude/CLAUDE.md`
  - `~/codes/.cursor/AGENTS.md` and `~/codes/.cursor/.cursorrules`
  - `~/codes/.chatgpt/AGENTS.md`
  - `~/codes/.openai/AGENTS.md`
  - `~/codes/.anthropic/AGENTS.md`

When this file and the parent disagree: follow **this file** for this MCP
server's tools, safety boundary, and env layout; follow the parent for org-wide
git/Linear/GitHub/k8s/shared-auth/opto-sync/ores-otel/zed-pkg conventions.

The mapping is 1:1:1:1 — GitHub org : Linear project : GitHub org project
(usually `https://github.com/orgs/<org>/projects/1`) : Slack channel in
`oresoftware-workspace.slack.com`. Linear workspace: https://linear.app/denman
Primary GitHub user: `ORESoftware`. Secondary: `the1mills`.


## This repository

- GitHub org: [`ores-otel`](https://github.com/ores-otel)
- Repository: [`ores-otel/ores-mcp-server-core-libs.rs`](https://github.com/ores-otel/ores-mcp-server-core-libs.rs)
- Local checkout: `~/codes/ores-otel/ores-mcp-server-core-libs.rs`
- Linear project: https://linear.app/denman/project/githubcomores-otel-85e70d77275a
- GitHub org project: https://github.com/orgs/ores-otel/projects/1
- Sibling test org: `github.com/ores-otel-test` (external/e2e coverage and extra GHA minutes)
- Slack workspace: oresoftware-workspace.slack.com
- Kind: product MCP server for this GitHub org
- Package / service name: `ores-mcp-server-core-libs`

Shared backends this org should lean on (see parent file for detail):

- `github.com/shared-auth` — dual auth
- `github.com/opto-sync` — cross-device sync
- `github.com/ores-otel` — logs, traces, metrics (stderr JSON + OTLP; stdout is MCP JSON-RPC only)
- `github.com/zed-pkg` — dependency intent (`.zpkg.toml`)
- `github.com/oresoftware/k8s-cluster` — deploy (except fiducia-cloud node/brain)

## MCP safety for this server

- Tools on this server: the product tool surface documented in README.md — do not add mutation, shell, or arbitrary-URL tools without an explicit safety review.
- stdout is the MCP wire. Diagnostics and telemetry go to stderr only.
- Never log, return, or serialize secret values. Capability checks report names/presence only.
- Do not add filesystem write, kubectl mutation, credentialed SSRF, or unauthenticated non-loopback HTTP without a dedicated review.
- Git: merge, never rebase/stash/reset unless a human explicitly authorizes. Resolve conflicts semantically.
- Build values, don't mutate them: functions return new values instead of filling `&mut` parameters or caller-owned collections. Deliberate exceptions on hot paths (inside a lock, streaming bodies, the accept loop) carry a `HOT-PATH (imperative by design)` comment with the reason. See [`FUNCTIONAL-STYLE.md`](./FUNCTIONAL-STYLE.md).

## Encrypted environment (sops + age + just + nix)

Secrets are committed as ciphertext only:

```
env/enc/dev.env.enc     committed ciphertext (source of truth)
env/enc/prod.env.enc    committed ciphertext (protected-operator recipients)
env/dec/*.env           gitignored plaintext, mode 0600, disposable
.env                    managed symlink into env/dec/ only
```

```sh
nix develop                 # or: direnv allow  (.envrc uses the flake)
just env-keygen             # once per machine
just env-decrypt            # env/enc -> env/dec
just env-use dev            # .env -> env/dec/dev.env
just env-run dev cargo test # no plaintext file
just env-check              # fail-closed; CI runs this
```

Private age keys live only in `~/Library/Application Support/sops/age/keys.txt`
(macOS) or `~/.config/sops/age/keys.txt` (Linux), mode 0600. `.just/env.just`
and `.just/dotenv.py` are the shared ores-sops module — keep them byte-identical
across the fleet; do not fork them in this repo.

## Required validation

```sh
just check          # if defined
just env-check
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

## Note
This crate is shared MCP **core libraries**, not a product stdio server. Keep stdout-wire rules in dependents.

## Repository-local Git worktrees

- Create or use a Git worktree only when the human operator explicitly authorizes it for the current task. Concurrency or a dirty checkout is not permission by itself.
- Put every authorized worktree at `<repository-root>/tmp/worktrees/<name>`; from the repository root, use `./tmp/worktrees/<name>`. Never place worktrees beside repositories or organization directories.
- Keep `tmp`, `temp`, `tmp/worktrees`, and `temp/worktrees` ignored in the repository-root `.gitignore`. Do not commit files from those directories.
- Relocate or remove a worktree only when the operator explicitly requests it. Before removal, preserve and publish intended changes, verify its commit is represented on the target branch, and confirm there are no tracked, untracked, ignored-sensitive, or in-use files that must survive. Remove it with `git worktree remove <path>` without `--force`; never delete a worktree directory with `rm`.

<!-- BEGIN ores-agents-pointer: managed by ORESoftware/my-ai; edit there, not here -->

## Canonical agent instructions

Before doing anything else in this repository, also read:

    .ores/agents/AGENTS.md

That path is a symlink to `~/codes/oresoftware/my-ai/AGENTS.md`, whose canonical copy is
<https://github.com/ORESoftware/my-ai/blob/main/AGENTS.md>.

It exists at a fixed path *inside* the repository because some agents cannot walk up past
the repository root, so machine-wide instructions one or more directories above are
invisible to them. This pointer plus that path make the same file reachable from a working
directory anywhere in the tree.

The symlink is deliberately **not committed**: it names an absolute path that is only valid
on a machine with `~/codes/oresoftware/my-ai` checked out, so committing it would produce a
broken link for everyone else and for CI. `.ores/` is git-ignored for that reason. If
`.ores/agents/AGENTS.md` is missing on your machine, create it with:

    mkdir -p .ores/agents
    ln -sfn "$HOME/codes/oresoftware/my-ai/AGENTS.md" .ores/agents/AGENTS.md

or run `~/codes/oresoftware/my-ai/scripts/link-repo-agents.sh` once to do it for every git
repository under `~/codes`, and `--check` to verify them.

A missing `.ores/agents/AGENTS.md` is a setup gap on the reader's machine, never a reason to
skip the canonical instructions: fetch them from the URL above instead.

<!-- END ores-agents-pointer -->
