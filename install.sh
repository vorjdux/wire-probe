#!/bin/sh
# wire-probe installer
# Usage: curl -sfL https://github.com/vorjdux/wire-probe/releases/latest/download/install.sh | sudo sh
# Or:    curl -sfL <url> | sh   (installs to ~/.local/bin when not root)
set -e

REPO="vorjdux/wire-probe"
BINARY="wire-probe"

# ── Resolve install directory ──────────────────────────────────────────────
if [ "$(id -u)" -eq 0 ]; then
  INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
else
  INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
  mkdir -p "$INSTALL_DIR"
fi

# ── Detect architecture ────────────────────────────────────────────────────
ARCH=$(uname -m)
case "$ARCH" in
  x86_64|amd64)           ARCH_SLUG="x86_64"  ;;
  aarch64|arm64|armv8*)   ARCH_SLUG="aarch64" ;;
  *)
    echo "error: unsupported architecture '$ARCH'" >&2
    echo "       Build from source: https://github.com/$REPO" >&2
    exit 1
    ;;
esac

# Static musl binary: works on Ubuntu, Fedora, Debian, RHEL, Alpine, etc.
ARTIFACT="${BINARY}-linux-${ARCH_SLUG}"

# ── Resolve latest version ─────────────────────────────────────────────────
if [ -z "$VERSION" ]; then
  VERSION=$(curl -sfL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
fi

if [ -z "$VERSION" ]; then
  echo "error: could not resolve latest release from GitHub API" >&2
  echo "       Set VERSION=vX.Y.Z to install a specific version" >&2
  exit 1
fi

BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

# ── Verify checksum tool availability ─────────────────────────────────────
if command -v sha256sum >/dev/null 2>&1; then
  SHA_CMD="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA_CMD="shasum -a 256"
else
  SHA_CMD=""
fi

# ── Download ───────────────────────────────────────────────────────────────
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Installing wire-probe ${VERSION} for linux/${ARCH_SLUG} → ${INSTALL_DIR}/${BINARY}"

curl --proto '=https' --tlsv1.2 -sfL \
  "${BASE_URL}/${ARTIFACT}" \
  -o "${TMP_DIR}/${BINARY}"

# ── Verify checksum (best-effort) ─────────────────────────────────────────
if [ -n "$SHA_CMD" ]; then
  curl --proto '=https' --tlsv1.2 -sfL \
    "${BASE_URL}/sha256sums.txt" \
    -o "${TMP_DIR}/sha256sums.txt" 2>/dev/null || true

  if [ -f "${TMP_DIR}/sha256sums.txt" ]; then
    EXPECTED=$(grep "${ARTIFACT}" "${TMP_DIR}/sha256sums.txt" | awk '{print $1}')
    if [ -n "$EXPECTED" ]; then
      ACTUAL=$(cd "$TMP_DIR" && $SHA_CMD "$BINARY" | awk '{print $1}')
      if [ "$ACTUAL" != "$EXPECTED" ]; then
        echo "error: checksum mismatch for ${ARTIFACT}" >&2
        echo "  expected: $EXPECTED" >&2
        echo "  actual:   $ACTUAL" >&2
        exit 1
      fi
      echo "Checksum OK: $ACTUAL"
    fi
  fi
fi

# ── Install ────────────────────────────────────────────────────────────────
chmod +x "${TMP_DIR}/${BINARY}"
mv "${TMP_DIR}/${BINARY}" "${INSTALL_DIR}/${BINARY}"

echo "Installed: ${INSTALL_DIR}/${BINARY}"

# PATH hint for non-root installs
if [ "$(id -u)" -ne 0 ]; then
  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
      echo ""
      echo "NOTE: ${INSTALL_DIR} is not in your PATH."
      echo "      Add it with:"
      echo "        echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.profile"
      ;;
  esac
fi

echo "Done. Run: ${BINARY} --mode server --port 9999"
