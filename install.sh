#!/bin/sh
# Cortex installer for Linux, macOS (and Git Bash on Windows).
#
#   curl -fsSL https://raw.githubusercontent.com/AstroQuestStudio/cortex/main/install.sh | sh
#
# What it does, and nothing else:
#   1. picks the release archive for your OS and CPU,
#   2. downloads it with SHA256SUMS.txt from the GitHub release,
#   3. refuses to continue unless the SHA-256 checksum matches,
#   4. copies the `cortex` binary into ~/.local/bin (never uses sudo),
#   5. tells you whether that folder is on your PATH. It does not edit your shell profile.
#
# Options (environment variables, or flags after `sh -s --`):
#   CORTEX_INSTALL_DIR / --dir DIR       install folder (default: ~/.local/bin)
#   CORTEX_VERSION     / --version X.Y.Z release to install (default: latest)
#
# Source: https://github.com/AstroQuestStudio/cortex (MIT). Made by AstroQuest.

set -eu

REPO="AstroQuestStudio/cortex"
INSTALL_DIR="${CORTEX_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"
VERSION="${CORTEX_VERSION:-latest}"

say() { printf '%s\n' "$*"; }
err() { printf 'cortex-install: error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --dir) [ $# -ge 2 ] || err "--dir needs a value"; INSTALL_DIR="$2"; shift 2 ;;
    --version) [ $# -ge 2 ] || err "--version needs a value"; VERSION="$2"; shift 2 ;;
    -h|--help) say "usage: install.sh [--dir DIR] [--version X.Y.Z]  (or CORTEX_INSTALL_DIR / CORTEX_VERSION)"; exit 0 ;;
    *) err "unknown option: $1 (use --dir DIR or --version X.Y.Z)" ;;
  esac
done

# --- platform -----------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *) err "no prebuilt binary for Linux $arch yet. Build from source: cargo install --git https://github.com/$REPO" ;;
    esac
    ext="tar.gz"; exe="cortex" ;;
  Darwin)
    # A shell running under Rosetta reports x86_64 on Apple silicon: prefer the native build.
    if [ "$arch" = "x86_64" ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = "1" ]; then
      arch="arm64"
    fi
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) err "unsupported macOS architecture: $arch" ;;
    esac
    ext="tar.gz"; exe="cortex" ;;
  MINGW*|MSYS*|CYGWIN*)
    case "$arch" in
      x86_64|amd64) target="x86_64-pc-windows-msvc" ;;
      *) err "unsupported Windows architecture: $arch" ;;
    esac
    ext="zip"; exe="cortex.exe" ;;
  *) err "unsupported OS: $os. Build from source: cargo install --git https://github.com/$REPO" ;;
esac

archive="cortex-$target.$ext"
case "$VERSION" in
  latest) base="https://github.com/$REPO/releases/latest/download" ;;
  v*) base="https://github.com/$REPO/releases/download/$VERSION" ;;
  *) base="https://github.com/$REPO/releases/download/v$VERSION" ;;
esac

# --- tools --------------------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  fetch() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q --https-only -O "$2" "$1"; }
else
  err "curl or wget is required"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
elif command -v openssl >/dev/null 2>&1; then
  sha256() { openssl dgst -sha256 "$1" | awk '{print $NF}'; }
else
  err "sha256sum, shasum or openssl is required to verify the download"
fi

if [ "$ext" = "zip" ]; then
  command -v unzip >/dev/null 2>&1 || err "unzip is required"
else
  command -v tar >/dev/null 2>&1 || err "tar is required"
fi

# --- download and verify ------------------------------------------------------
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t cortex-install)"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT TERM

say "Cortex installer: $archive ($VERSION)"
say "  downloading from $base/"
fetch "$base/$archive" "$tmp/$archive" || err "download failed: $base/$archive"
fetch "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" || err "download failed: $base/SHA256SUMS.txt"

expected="$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS.txt")"
[ -n "$expected" ] || err "$archive is not listed in SHA256SUMS.txt"
actual="$(sha256 "$tmp/$archive")"
if [ "$actual" != "$expected" ]; then
  err "checksum mismatch for $archive (expected $expected, got $actual). Nothing was installed."
fi
say "  sha256 verified: $actual"

mkdir -p "$tmp/x"
# Extract the whole (small) archive: member names may or may not start with "./".
if [ "$ext" = "zip" ]; then
  unzip -q -o "$tmp/$archive" -d "$tmp/x"
else
  tar -xzf "$tmp/$archive" -C "$tmp/x"
fi
[ -f "$tmp/x/$exe" ] || err "$exe not found in $archive"
chmod 755 "$tmp/x/$exe"

if ! installed_version="$("$tmp/x/$exe" --version 2>/dev/null)"; then
  if [ "$os" = "Linux" ]; then
    err "the downloaded binary does not run here (the Linux build needs glibc 2.35+, e.g. Ubuntu 22.04 or Debian 12). Build from source: cargo install --git https://github.com/$REPO"
  fi
  err "the downloaded binary does not run on this system"
fi

# --- install ------------------------------------------------------------------
mkdir -p "$INSTALL_DIR" 2>/dev/null || err "cannot create $INSTALL_DIR. Choose another folder: CORTEX_INSTALL_DIR=/some/dir"
[ -w "$INSTALL_DIR" ] || err "$INSTALL_DIR is not writable. Choose another folder with CORTEX_INSTALL_DIR (this script never runs sudo)"
# Copy next to the target, then rename: an upgrade never leaves a half-written binary.
cp "$tmp/x/$exe" "$INSTALL_DIR/.$exe.new"
mv -f "$INSTALL_DIR/.$exe.new" "$INSTALL_DIR/$exe"

say "  installed $installed_version -> $INSTALL_DIR/$exe"
say ""

case ":$PATH:" in
  *":$INSTALL_DIR:"*) on_path=1 ;;
  *) on_path=0 ;;
esac
if [ "$on_path" -eq 0 ]; then
  say "$INSTALL_DIR is not on your PATH. Add this line to your shell profile (~/.bashrc, ~/.zshrc...):"
  say "  export PATH=\"$INSTALL_DIR:\$PATH\""
  say ""
fi

say "Next steps:"
say "  cd your-project && cortex index . --name MyProject"
say "  cortex find \"where are sessions signed\""
say "  claude mcp add --scope user cortex -- cortex mcp    # or any MCP client: command \"cortex\", args [\"mcp\"]"
say ""
say "Docs: https://github.com/$REPO#readme"
