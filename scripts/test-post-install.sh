#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/test-post-install.sh <bash|zsh|fish> [options]

Exercise the scpr plugin post_install hook in a disposable HOME so shell
completion setup can be inspected safely.

Options:
  --with-loader  Pre-seed the shell rc file so it already loads ~/.bashrc.d or
                 ~/.zshrc.d before the install runs.
  --keep-temp    Keep the temporary HOME directory instead of deleting it.
  --no-build     Skip cargo build and reuse the existing target/debug/scpr.
  -h, --help     Show this help message.

Examples:
  scripts/test-post-install.sh bash
  scripts/test-post-install.sh bash --with-loader
  scripts/test-post-install.sh zsh
  scripts/test-post-install.sh fish
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

print_file_if_exists() {
  local path="$1"
  if [[ -f "$path" ]]; then
    printf '\n==> %s <==\n' "$path"
    cat "$path"
  fi
}

seed_loader() {
  local shell_name="$1"

  case "$shell_name" in
    bash)
      cat >"$HOME/.bashrc" <<'EOF'
for f in "$HOME/.bashrc.d/"*.sh; do
  [ -r "$f" ] && . "$f"
done
EOF
      ;;
    zsh)
      cat >"$HOME/.zshrc" <<'EOF'
for f in "$HOME/.zshrc.d/"*.sh; do
  [ -r "$f" ] && . "$f"
done
EOF
      ;;
    fish)
      ;;
    *)
      die "unsupported shell for loader setup: $shell_name"
      ;;
  esac
}

main() {
  (($# >= 1)) || {
    usage
    exit 1
  }

  local shell_name="$1"
  shift

  local with_loader=0
  local keep_temp=0
  local run_build=1

  while (($# > 0)); do
    case "$1" in
      --with-loader)
        with_loader=1
        ;;
      --keep-temp)
        keep_temp=1
        ;;
      --no-build)
        run_build=0
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown option: $1"
        ;;
    esac
    shift
  done

  case "$shell_name" in
    bash|zsh|fish)
      ;;
    *)
      die "shell must be one of: bash, zsh, fish"
      ;;
  esac

  need_cmd cargo
  need_cmd git
  need_cmd mktemp

  local repo_root
  repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "must be run inside the repository"
  cd "$repo_root"

  if ((run_build)); then
    cargo build
  fi

  [[ -x ./target/debug/scpr ]] || die "expected ./target/debug/scpr to exist; run without --no-build or build it first"

  local tmp_root
  tmp_root="$(mktemp -d)"
  export HOME="$tmp_root/home"
  export XDG_CONFIG_HOME="$HOME/.config"
  export XDG_DATA_HOME="$HOME/.local/share"
  export SCPR_BIN_DIR="$HOME/.local/bin"
  export SCPR_MAN_DIR="$HOME/.local/share/man/man1"

  mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$SCPR_BIN_DIR" "$SCPR_MAN_DIR"

  if ((with_loader)); then
    seed_loader "$shell_name"
  fi

  trap 'if [[ ${keep_temp} -eq 0 ]]; then rm -rf "$tmp_root"; else printf "Kept temp directory: %s\n" "$tmp_root"; fi' EXIT

  local shell_path
  case "$shell_name" in
    bash)
      shell_path="/bin/bash"
      ;;
    zsh)
      shell_path="/bin/zsh"
      ;;
    fish)
      shell_path="/usr/bin/fish"
      ;;
  esac

  printf 'Testing post_install for %s in %s\n' "$shell_name" "$HOME"
  SHELL="$shell_path" ./target/debug/scpr install scpr --plugins-dir ./plugins

  case "$shell_name" in
    bash)
      print_file_if_exists "$HOME/.bashrc.d/scpr.sh"
      print_file_if_exists "$HOME/.bashrc"
      ;;
    zsh)
      print_file_if_exists "$HOME/.zshrc.d/scpr.sh"
      print_file_if_exists "$HOME/.zshrc"
      ;;
    fish)
      print_file_if_exists "$HOME/.config/fish/conf.d/scpr.fish"
      ;;
  esac

  printf '\nRe-running install to check idempotency...\n'
  SHELL="$shell_path" ./target/debug/scpr install scpr --plugins-dir ./plugins

  case "$shell_name" in
    bash)
      print_file_if_exists "$HOME/.bashrc"
      ;;
    zsh)
      print_file_if_exists "$HOME/.zshrc"
      ;;
  esac
}

main "$@"
