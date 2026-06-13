#!/bin/sh
# wire-probe collectd plugin installer
#
# Usage:
#   curl -sSf https://raw.githubusercontent.com/vorjdux/wire-probe/main/install-plugin.sh | sh
#
# Environment overrides:
#   PLUGIN_DIR=/usr/lib/collectd/wire_probe   override plugin install directory
#   CONF_DIR=/etc/collectd/conf.d             override collectd config drop-in directory
#   NO_RELOAD=1                               skip 'systemctl reload collectd'
#   NO_COLOR=1                                disable coloured output
set -e

REPO="vorjdux/wire-probe"
RAW_BASE="https://raw.githubusercontent.com/${REPO}/main/plugin/collectd"

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
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --help|-h)
      echo "Usage: install-plugin.sh [--dry-run]"
      echo ""
      echo "Environment variables:"
      echo "  PLUGIN_DIR=/path    directory to install wire_probe.py"
      echo "  CONF_DIR=/path      collectd conf.d directory"
      echo "  NO_RELOAD=1         skip systemctl reload collectd"
      exit 0 ;;
  esac
done

# ── Dependency check ───────────────────────────────────────────────────────
command -v curl >/dev/null 2>&1 || die "'curl' is required but not installed"

# ── Paths ──────────────────────────────────────────────────────────────────
PLUGIN_DIR="${PLUGIN_DIR:-/usr/lib/collectd/wire_probe}"
CONF_DIR="${CONF_DIR:-/etc/collectd/conf.d}"

info "Installing wire-probe collectd plugin"
info "  plugin: ${PLUGIN_DIR}/wire_probe.py"
info "  config: ${CONF_DIR}/wire_probe.conf  (skipped if already present)"

[ "$DRY_RUN" -eq 1 ] && { ok "dry-run: nothing downloaded"; exit 0; }

# ── Root check ─────────────────────────────────────────────────────────────
[ "$(id -u)" -eq 0 ] || die "must be run as root (sudo) to write to ${PLUGIN_DIR} and ${CONF_DIR}"

# ── Install plugin ─────────────────────────────────────────────────────────
mkdir -p "$PLUGIN_DIR"
curl --proto '=https' --tlsv1.2 -sfL \
  "${RAW_BASE}/wire_probe.py" \
  -o "${PLUGIN_DIR}/wire_probe.py" \
  || die "failed to download wire_probe.py"
ok "installed ${PLUGIN_DIR}/wire_probe.py"

# ── Install example config (only if absent) ────────────────────────────────
if [ -f "${CONF_DIR}/wire_probe.conf" ]; then
  warn "${CONF_DIR}/wire_probe.conf already exists  -  not overwriting"
else
  mkdir -p "$CONF_DIR"
  curl --proto '=https' --tlsv1.2 -sfL \
    "${RAW_BASE}/wire_probe.conf" \
    -o "${CONF_DIR}/wire_probe.conf" \
    || die "failed to download wire_probe.conf"
  ok "installed ${CONF_DIR}/wire_probe.conf"
  warn "Edit ${CONF_DIR}/wire_probe.conf to set your target hosts, then reload collectd"
fi

# ── Reload collectd ────────────────────────────────────────────────────────
if [ -z "${NO_RELOAD:-}" ]; then
  if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet collectd 2>/dev/null; then
    systemctl reload collectd
    ok "collectd reloaded"
  else
    warn "collectd does not appear to be running  -  start it manually when ready"
  fi
fi

ok "Done  -  see ${CONF_DIR}/wire_probe.conf to configure target hosts"
