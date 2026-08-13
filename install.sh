#!/bin/sh
# wire-probe installer
#
# Usage:
#   curl -sSf https://raw.githubusercontent.com/vorjdux/wire-probe/main/install.sh | sh
#
# Environment overrides:
#   VERSION=0.1.0   install a specific version (without the 'v' prefix)
#   INSTALL_DIR=/usr/local/bin   override install location
#   NO_COLOR=1      disable coloured output
#   INSECURE=1      skip SHA256 checksum verification (not recommended)
set -e

REPO="vorjdux/wire-probe"
BINARY="wire-probe"

# ── Colour output ──────────────────────────────────────────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  GREEN='\033[0;32m'; YELLOW='\033[1;33m'
  RED='\033[0;31m';   BOLD='\033[1m';  RESET='\033[0m'
else
  GREEN=''; YELLOW=''; RED=''; BOLD=''; RESET=''
fi

info()  { printf "${BOLD}%s${RESET}\n"   "$*"; }
ok()    { printf "${GREEN}✓ %s${RESET}\n" "$*"; }
warn()  { printf "${YELLOW}! %s${RESET}\n" "$*" >&2; }
die()   { printf "${RED}error: %s${RESET}\n" "$*" >&2; exit 1; }

# ── Argument parsing ───────────────────────────────────────────────────────
DRY_RUN=0
NO_MODIFY_PATH=0
for arg in "$@"; do
  case "$arg" in
    --dry-run)        DRY_RUN=1 ;;
    --no-modify-path) NO_MODIFY_PATH=1 ;;
    --help|-h)
      echo "Usage: install.sh [--dry-run] [--no-modify-path]"
      echo ""
      echo "Environment variables:"
      echo "  VERSION=0.1.0       install a specific version"
      echo "  INSECURE=1          skip checksum verification"
      echo "  INSTALL_DIR=/path   override install directory"
      exit 0 ;;
  esac
done

# ── Dependency check ───────────────────────────────────────────────────────
for cmd in curl tar; do
  command -v "$cmd" >/dev/null 2>&1 || die "'$cmd' is required but not installed"
done

if command -v sha256sum >/dev/null 2>&1; then
  SHA256_CMD="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA256_CMD="shasum -a 256"
else
  # Not fatal here: the verification block below decides, so that INSECURE=1
  # still works on a host without either tool.
  SHA256_CMD=""
fi

# ── Platform detection ─────────────────────────────────────────────────────
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$OS" in
  linux)  OS="linux" ;;
  *)      die "unsupported OS '$OS'  -  only Linux is supported" ;;
esac

ARCH=$(uname -m)
case "$ARCH" in
  x86_64|amd64)           ARCH="x86_64"  ;;
  aarch64|arm64|armv8*)   ARCH="aarch64" ;;
  *)      die "unsupported architecture '$ARCH'" ;;
esac

# ── Version resolution ─────────────────────────────────────────────────────
if [ -z "${VERSION:-}" ]; then
  VERSION=$(curl -sfL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 \
    | sed 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/')
  [ -n "$VERSION" ] || die "could not resolve latest version from GitHub API"
fi

# ── Paths ──────────────────────────────────────────────────────────────────
if [ -z "${INSTALL_DIR:-}" ]; then
  if [ "$(id -u)" -eq 0 ]; then
    INSTALL_DIR="/usr/local/bin"
  else
    INSTALL_DIR="$HOME/.local/bin"
  fi
fi
# Applies to a caller-supplied INSTALL_DIR too: without this, a directory that
# does not exist yet surfaces as a cryptic mv failure after the download.
mkdir -p "$INSTALL_DIR" || die "cannot create install directory ${INSTALL_DIR}"

ARCHIVE_NAME="${BINARY}-${VERSION}-${OS}-${ARCH}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"
ARCHIVE_URL="${BASE_URL}/${ARCHIVE_NAME}"
SUMS_URL="${BASE_URL}/SHA256SUMS"

# ── Summary ────────────────────────────────────────────────────────────────
info "Installing ${BINARY} v${VERSION} (${OS}/${ARCH})"
info "  from:  ${ARCHIVE_URL}"
info "  into:  ${INSTALL_DIR}"

[ "$DRY_RUN" -eq 1 ] && { ok "dry-run: nothing downloaded"; exit 0; }

# ── Download ───────────────────────────────────────────────────────────────
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

ARCHIVE_PATH="${TMP_DIR}/${ARCHIVE_NAME}"

curl --proto '=https' --tlsv1.2 -sfL "$ARCHIVE_URL" -o "$ARCHIVE_PATH" \
  || die "download failed: ${ARCHIVE_URL}"

# ── Checksum verification ──────────────────────────────────────────────────
# Fail closed. Every release publishes SHA256SUMS, so a missing or incomplete
# one means something is wrong with the download path, and this script pipes
# a downloaded binary straight into a system directory. INSECURE=1 opts out.
if [ -n "${INSECURE:-}" ]; then
  warn "INSECURE=1  -  skipping checksum verification"
elif [ -z "$SHA256_CMD" ]; then
  die "no sha256 tool found (sha256sum or shasum)  -  install one, or re-run with INSECURE=1"
else
  SUMS_PATH="${TMP_DIR}/SHA256SUMS"
  curl --proto '=https' --tlsv1.2 -sfL "$SUMS_URL" -o "$SUMS_PATH" 2>/dev/null \
    || die "cannot download SHA256SUMS from ${SUMS_URL}  -  re-run with INSECURE=1 to skip"

  EXPECTED=$(grep "${ARCHIVE_NAME}" "$SUMS_PATH" | cut -d' ' -f1)
  [ -n "$EXPECTED" ] \
    || die "${ARCHIVE_NAME} is not listed in SHA256SUMS  -  re-run with INSECURE=1 to skip"

  ACTUAL=$($SHA256_CMD "$ARCHIVE_PATH" | cut -d' ' -f1)
  [ "$ACTUAL" = "$EXPECTED" ] || die "checksum mismatch for ${ARCHIVE_NAME}
  expected: ${EXPECTED}
  actual:   ${ACTUAL}"
  ok "checksum verified"
fi

# ── Extract and install ────────────────────────────────────────────────────
tar xzf "$ARCHIVE_PATH" -C "$TMP_DIR"
EXTRACTED=$(find "$TMP_DIR" -name "$BINARY" -type f | head -1)
[ -n "$EXTRACTED" ] || die "binary '${BINARY}' not found in archive"

chmod +x "$EXTRACTED"
mv "$EXTRACTED" "${INSTALL_DIR}/${BINARY}"

ok "installed ${INSTALL_DIR}/${BINARY}"

# ── PATH hint ──────────────────────────────────────────────────────────────
if [ "$NO_MODIFY_PATH" -eq 0 ] && [ "$(id -u)" -ne 0 ]; then
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
      warn "${INSTALL_DIR} is not in your PATH. Add it with:"
      printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.profile\n' "$INSTALL_DIR"
      ;;
  esac
fi

ok "Done  -  run: ${BINARY} --mode server --port 9999"
