# Encrypted dotenv workflow

Only SOPS-encrypted dotenv artifacts under `env/enc/*.env.enc` may be
committed. Plaintext files under `env/dec/`, the local `.sops.yaml`, and every
age private identity are ignored by Git.

The recipes use this one-level mapping:

| Plaintext (local only) | Ciphertext (committable) |
| --- | --- |
| `env/dec/<profile>.env` | `env/enc/<profile>.env.enc` |

Profile names must match `[A-Za-z0-9][A-Za-z0-9._-]*`. Files in
subdirectories and symlinks are rejected or not processed.

## First-time setup

1. Enter `nix develop` to obtain Rust 1.88, SOPS, age, and just.
   `flake.nix` pins its source inputs to immutable commits; commit the
   generated `flake.lock` after the first successful Nix evaluation.
2. Copy `.sops.yaml.example` to `.sops.yaml` and replace the placeholder with
   the team's **public** age recipient. The copied policy remains local.
3. Keep the matching private identity outside the repository. If a local file
   is necessary, place it at `.age/keys.txt`, set mode `0600`, and export
   `SOPS_AGE_KEY_FILE="$PWD/.age/keys.txt"`. The entire `.age/` directory is
   ignored.
4. Create `env/dec/local.env` with mode `0600`, then run `just encrypt-all`.
5. Commit only the resulting `env/enc/local.env.enc` artifact.

Collaborators with a matching age identity can run `just decrypt-all`. Both
recipes set `umask 077`, disable shell tracing, use same-directory temporary
files and atomic renames, and quote every filesystem-derived name. On success
they print only a file count; SOPS diagnostics are suppressed so a parse error
cannot echo plaintext. A failure removes the partial temporary file and leaves
that file's previous destination intact.

SOPS encrypts dotenv values, but variable names and file profile names remain
visible metadata. Do not place sensitive values in filenames or variable
names, and never paste secret values into command arguments, logs, issues, or
pull requests. Rotate a disclosed credential immediately; re-encryption alone
does not revoke it.
