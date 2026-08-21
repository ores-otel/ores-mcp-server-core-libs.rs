default:
    @just --list

# Encrypt every direct env/dec/<profile>.env file into env/enc/<profile>.env.enc.
encrypt-all:
    #!/usr/bin/env bash
    set -euo pipefail
    set +x
    umask 077

    command -v sops >/dev/null 2>&1 || {
      printf '%s\n' 'sops is required; run this recipe from `nix develop`.' >&2
      exit 1
    }
    [[ -f .sops.yaml && ! -L .sops.yaml ]] || {
      printf '%s\n' 'Create the ignored .sops.yaml from .sops.yaml.example first.' >&2
      exit 1
    }
    if grep -q 'age1replace' .sops.yaml; then
      printf '%s\n' 'Replace the example age recipient in .sops.yaml first.' >&2
      exit 1
    fi
    [[ -d env/dec && ! -L env/dec && -d env/enc && ! -L env/enc ]] || {
      printf '%s\n' 'env/dec and env/enc must be real directories, not symlinks.' >&2
      exit 1
    }

    temporary=''
    cleanup() {
      if [[ -n "${temporary:-}" ]]; then
        rm -f "$temporary"
      fi
    }
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    count=0
    while IFS= read -r -d '' input; do
      filename="${input##*/}"
      profile="${filename%.env}"
      [[ "$profile" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || {
        printf '%s\n' 'A plaintext profile filename violates the required policy.' >&2
        exit 1
      }
      output="env/enc/${profile}.env.enc"
      if [[ -L "$output" || ( -e "$output" && ! -f "$output" ) ]]; then
        printf '%s\n' 'An encrypted destination is not a regular file.' >&2
        exit 1
      fi
      temporary="$(mktemp 'env/enc/.encrypt.XXXXXX')"
      chmod 600 "$temporary"
      if ! sops encrypt \
        --input-type dotenv \
        --output-type dotenv \
        --filename-override "$output" \
        "$input" >"$temporary" 2>/dev/null; then
        printf '%s\n' 'SOPS encryption failed; destination was not replaced.' >&2
        exit 1
      fi
      mv -f "$temporary" "$output"
      temporary=''
      count=$((count + 1))
    done < <(find env/dec -mindepth 1 -maxdepth 1 -type f -name '?*.env' -print0)

    trap - EXIT HUP INT TERM
    printf 'Encrypted %d file(s).\n' "$count"

# Decrypt every direct env/enc/<profile>.env.enc file into env/dec/<profile>.env.
decrypt-all:
    #!/usr/bin/env bash
    set -euo pipefail
    set +x
    umask 077

    command -v sops >/dev/null 2>&1 || {
      printf '%s\n' 'sops is required; run this recipe from `nix develop`.' >&2
      exit 1
    }
    [[ -d env/dec && ! -L env/dec && -d env/enc && ! -L env/enc ]] || {
      printf '%s\n' 'env/dec and env/enc must be real directories, not symlinks.' >&2
      exit 1
    }

    temporary=''
    cleanup() {
      if [[ -n "${temporary:-}" ]]; then
        rm -f "$temporary"
      fi
    }
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    count=0
    while IFS= read -r -d '' input; do
      filename="${input##*/}"
      profile="${filename%.env.enc}"
      [[ "$profile" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || {
        printf '%s\n' 'An encrypted profile filename violates the required policy.' >&2
        exit 1
      }
      output="env/dec/${profile}.env"
      if [[ -L "$output" || ( -e "$output" && ! -f "$output" ) ]]; then
        printf '%s\n' 'A decrypted destination is not a regular file.' >&2
        exit 1
      fi
      temporary="$(mktemp 'env/dec/.decrypt.XXXXXX')"
      chmod 600 "$temporary"
      if ! sops decrypt \
        --input-type dotenv \
        --output-type dotenv \
        "$input" >"$temporary" 2>/dev/null; then
        printf '%s\n' 'SOPS decryption failed; destination was not replaced.' >&2
        exit 1
      fi
      mv -f "$temporary" "$output"
      temporary=''
      count=$((count + 1))
    done < <(find env/enc -mindepth 1 -maxdepth 1 -type f -name '?*.env.enc' -print0)

    trap - EXIT HUP INT TERM
    printf 'Decrypted %d file(s).\n' "$count"
