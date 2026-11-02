#!/usr/bin/env bash

set -euo pipefail

schema_tmp=""

cleanup() {
  if [[ -n "$schema_tmp" ]]; then
    rm -f "$schema_tmp"
  fi
}

trap cleanup EXIT

usage() {
  cat <<'EOF'
Usage: scripts/validate-plugins.sh [path] [options]

Validate one plugin TOML file or every top-level TOML file in a plugin
directory. With no path, the bundled `plugins` directory is validated.

Arguments:
  path                 Plugin TOML file or directory (default: plugins)

Options:
  --write-schema       Regenerate schemas/plugin.schema.json from the Rust types.
  --check-schema       Fail if schemas/plugin.schema.json is out of date.
  --no-build           Reuse target/debug/scpr instead of building first.
  -h, --help           Show this help message.

Environment:
  SCPR_BIN             Path to an existing scpr binary. When set, the script
                       does not build and ignores --no-build.

Examples:
  scripts/validate-plugins.sh
  scripts/validate-plugins.sh plugins/mdbook.toml
  scripts/validate-plugins.sh /path/to/plugins
  scripts/validate-plugins.sh --check-schema
  scripts/validate-plugins.sh --write-schema
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

main() {
  local target="plugins"
  local have_target=0
  local write_schema=0
  local check_schema=0
  local run_build=1

  while (($# > 0)); do
    case "$1" in
      --write-schema)
        write_schema=1
        ;;
      --check-schema)
        check_schema=1
        ;;
      --no-build)
        run_build=0
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      -*)
        die "unknown option: $1"
        ;;
      *)
        ((have_target == 0)) || die "only one path may be provided"
        target="$1"
        have_target=1
        ;;
    esac
    shift
  done

  ((write_schema == 0 || check_schema == 0)) ||
    die "use either --write-schema or --check-schema, not both"

  need_cmd git
  local repo_root
  repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" ||
    die "must be run inside a git repository"
  cd "$repo_root"

  local schema_path="$repo_root/schemas/plugin.schema.json"
  local scpr_bin="${SCPR_BIN:-$repo_root/target/debug/scpr}"

  if ((run_build)) && [[ -z "${SCPR_BIN:-}" ]]; then
    need_cmd cargo
    cargo build --quiet
  fi
  [[ -x "$scpr_bin" ]] ||
    die "expected $scpr_bin to exist; build it first or run without --no-build"

  if ((write_schema)); then
    "$scpr_bin" plugins schema --output "$schema_path"
    return
  fi

  if ((check_schema)); then
    need_cmd cmp
    need_cmd mktemp

    schema_tmp="$(mktemp)"

    "$scpr_bin" plugins schema --output "$schema_tmp" >/dev/null
    if ! cmp -s "$schema_tmp" "$schema_path"; then
      die "schemas/plugin.schema.json is out of date; run scripts/validate-plugins.sh --write-schema"
    fi
    printf 'schema is up to date: schemas/plugin.schema.json\n'
    return
  fi

  local -a files=()
  if [[ -d "$target" ]]; then
    shopt -s nullglob
    files=("$target"/*.toml)
    shopt -u nullglob
    ((${#files[@]} > 0)) || die "no .toml files found in $target"
  elif [[ -f "$target" ]]; then
    files=("$target")
  else
    die "path not found: $target"
  fi

  local failed=0
  local file
  for file in "${files[@]}"; do
    if ! "$scpr_bin" plugins validate "$file"; then
      printf 'invalid: %s\n' "$file" >&2
      failed=$((failed + 1))
    fi
  done

  if ((failed > 0)); then
    die "$failed of ${#files[@]} plugin file(s) failed validation"
  fi

  printf 'validated %d plugin file(s)\n' "${#files[@]}"
}

main "$@"
