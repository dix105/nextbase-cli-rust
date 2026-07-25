#!/usr/bin/env bash
# Nextbase CLI (Wisper) installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/dix105/nextbase-cli-rust/main/install.sh | bash
#
# Prefers a prebuilt binary, so neither git nor a Rust toolchain is needed. Falls
# back to building from source only if no binary exists for this platform.
set -euo pipefail

REPO="${WISPER_REPO:-dix105/nextbase-cli-rust}"
BIN_DIR="${WISPER_BIN_DIR:-$HOME/.local/bin}"
VERSION="${WISPER_VERSION:-latest}"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

say()  { printf '%s\n' "$*"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '\033[33m!\033[0m %s\n' "$*"; }
die()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"; }
need curl
need tar
need uname

# ----------------------------------------------------------------- platform

case "$(uname -s)" in
  Darwin) OS=apple-darwin ;;
  Linux)  OS=unknown-linux-gnu ;;
  *)      die "Unsupported system: $(uname -s). macOS and Linux only; use install.ps1 on Windows." ;;
esac

case "$(uname -m)" in
  arm64|aarch64) ARCH=aarch64 ;;
  x86_64|amd64)  ARCH=x86_64 ;;
  *)             die "Unsupported architecture: $(uname -m)" ;;
esac

TARGET="${ARCH}-${OS}"

# ------------------------------------------------------- stop what's running

# A listener holds the old binary and would keep running the old code after the
# swap. Ask the installed CLI to stop first, then fall back to the PID file.
stop_listener() {
  if command -v wisper >/dev/null 2>&1; then
    wisper stop >/dev/null 2>&1 || true
  fi
  local pid_file="$HOME/.wisper-cli/listener.pid"
  [ -f "$pid_file" ] || return 0

  local pid
  pid="$(cat "$pid_file" 2>/dev/null || true)"
  case "$pid" in
    ''|*[!0-9]*) return 0 ;;
  esac

  # A stale PID may have been recycled by an unrelated process, so confirm what it
  # is before signalling it.
  local cmd
  cmd="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  case "$cmd" in
    *wisper*|*nextbase*|*cli.js*) kill "$pid" 2>/dev/null || true ;;
  esac
}

# ------------------------------------------------------------- prebuilt path

asset_url() {
  local base="https://github.com/$REPO/releases"
  if [ "$VERSION" = "latest" ]; then
    printf '%s/latest/download/nextbase-wisper-%s.tar.gz' "$base" "$TARGET"
  else
    printf '%s/download/%s/nextbase-wisper-%s.tar.gz' "$base" "$VERSION" "$TARGET"
  fi
}

install_prebuilt() {
  local url archive
  url="$(asset_url)"
  archive="$TMP_DIR/wisper.tar.gz"

  say "Looking for a prebuilt binary for $TARGET..."
  if ! curl -fsSL --retry 2 -o "$archive" "$url" 2>/dev/null; then
    return 1
  fi

  tar -xzf "$archive" -C "$TMP_DIR" || return 1
  [ -f "$TMP_DIR/wisper" ] || return 1

  mkdir -p "$BIN_DIR"
  stop_listener
  install -m 755 "$TMP_DIR/wisper" "$BIN_DIR/wisper"
  [ -f "$TMP_DIR/nextbase" ] && install -m 755 "$TMP_DIR/nextbase" "$BIN_DIR/nextbase"

  # Downloaded binaries are quarantined until signing and notarization are set up.
  if [ "$OS" = "apple-darwin" ]; then
    xattr -d com.apple.quarantine "$BIN_DIR/wisper" 2>/dev/null || true
    xattr -d com.apple.quarantine "$BIN_DIR/nextbase" 2>/dev/null || true
  fi
  return 0
}

# --------------------------------------------------------------- source path

install_from_source() {
  command -v cargo >/dev/null 2>&1 || return 1

  say "No prebuilt binary available. Building from source with cargo..."
  # Source tarball instead of `git clone`, so git is not required.
  local branch="${WISPER_BRANCH:-main}"
  if ! curl -fsSL "https://codeload.github.com/$REPO/tar.gz/refs/heads/$branch" \
       -o "$TMP_DIR/src.tar.gz"; then
    return 1
  fi
  tar -xzf "$TMP_DIR/src.tar.gz" -C "$TMP_DIR" || return 1

  local src
  src="$(find "$TMP_DIR" -maxdepth 1 -type d -name 'nextbase-cli-rust-*' | head -1)"
  [ -n "$src" ] || return 1

  stop_listener
  ( cd "$src" && cargo install --path crates/nextbase-cli --locked --root "$TMP_DIR/out" ) || return 1

  mkdir -p "$BIN_DIR"
  install -m 755 "$TMP_DIR/out/bin/wisper" "$BIN_DIR/wisper"
  install -m 755 "$TMP_DIR/out/bin/nextbase" "$BIN_DIR/nextbase"
  return 0
}

# ------------------------------------------------------------------- install

# Keep a TypeScript install reachable instead of overwriting it: the two builds
# share ~/.wisper-cli, and having the old one still callable makes rollback easy.
if [ -e "$BIN_DIR/wisper" ] && ! "$BIN_DIR/wisper" --version 2>/dev/null | grep -q '^wisper '; then
  mv -f "$BIN_DIR/wisper" "$BIN_DIR/wisper-ts"
  warn "Existing wisper moved to $BIN_DIR/wisper-ts (the previous build is still callable as wisper-ts)."
fi

if install_prebuilt; then
  ok "Installed a prebuilt binary."
elif install_from_source; then
  ok "Built and installed from source."
else
  say ""
  die "Could not install.
No prebuilt binary exists for $TARGET yet, and building from source needs cargo.
Install Rust and re-run this script:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

command -v "$BIN_DIR/wisper" >/dev/null 2>&1 || die "Install finished but $BIN_DIR/wisper is missing."
INSTALLED="$("$BIN_DIR/wisper" --version 2>/dev/null || echo unknown)"
ok "$INSTALLED at $BIN_DIR/wisper"

# ---------------------------------------------------------------------- PATH

case ":$PATH:" in
  *":$BIN_DIR:"*) ON_PATH=1 ;;
  *)              ON_PATH=0 ;;
esac

if [ "$ON_PATH" = "0" ]; then
  if [ "${SHELL##*/}" = "zsh" ]; then
    PROFILE="${ZDOTDIR:-$HOME}/.zshrc"
  else
    PROFILE="$HOME/.bashrc"
  fi
  LINE="export PATH=\"$BIN_DIR:\$PATH\""
  touch "$PROFILE"
  grep -Fqx "$LINE" "$PROFILE" 2>/dev/null || printf '\n# Nextbase CLI\n%s\n' "$LINE" >> "$PROFILE"
  warn "Added $BIN_DIR to $PROFILE. Open a new terminal, or run: $LINE"
else
  # Another wisper earlier on PATH would silently win.
  RESOLVED="$(command -v wisper 2>/dev/null || true)"
  if [ -n "$RESOLVED" ] && [ "$RESOLVED" != "$BIN_DIR/wisper" ]; then
    warn "A different wisper is earlier on PATH: $RESOLVED"
    warn "Run this build explicitly as $BIN_DIR/wisper, or put $BIN_DIR first on PATH."
  fi
fi

say ""
say "Next:"
say "  wisper setup     Choose a model, paste an API key, pick a shortcut"
say "  wisper doctor    Check permissions, microphone, and shortcuts"
say "  wisper listen    Start the background listener"

if [ "$OS" = "apple-darwin" ]; then
  say ""
  warn "macOS needs Accessibility permission for global shortcuts:"
  say "  System Settings > Privacy & Security > Accessibility, then add:"
  say "  $BIN_DIR/wisper"
  say "  The grant is tied to this exact binary, so re-run this after any update."
fi
