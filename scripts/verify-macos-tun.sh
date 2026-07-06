#!/usr/bin/env bash
set -euo pipefail

APP_SUPPORT="${HOME}/Library/Application Support/org.air.Air"
CACHE_DIR="${HOME}/Library/Caches/org.air.Air/core"
CONFIG_PATH="${APP_SUPPORT}/core.common.config.yaml"
MIHOMO="${CACHE_DIR}/mihomo"
LOG_PATH="${APP_SUPPORT}/logs/core.verify-tun.log"
PID_FILE="${TMPDIR:-/tmp}/air-mihomo-verify-tun.pid"
CONTROLLER_FALLBACK="127.0.0.1:19090"
ADMIN_TIMEOUT_SECONDS="${AIR_TUN_ADMIN_TIMEOUT_SECONDS:-180}"

usage() {
  cat <<'USAGE'
Usage: scripts/verify-macos-tun.sh [--check-only]
       scripts/verify-macos-tun.sh [--sudo]

Validates the real macOS Air mihomo files, then starts mihomo with macOS
administrator privileges, checks the controller and utun interface, and stops it.

Options:
  --check-only  Validate files and mihomo config without requesting privileges.
  --sudo        Request administrator privileges through sudo in the terminal
                instead of an AppleScript authorization dialog.
  -h, --help    Show this help.
USAGE
}

CHECK_ONLY=0
ADMIN_MODE="osascript"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --check-only)
      CHECK_ONLY=1
      ;;
    --sudo)
      ADMIN_MODE="sudo"
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This verifier only runs on macOS." >&2
  exit 1
fi

if [[ ! -x "${MIHOMO}" ]]; then
  echo "Missing executable mihomo at ${MIHOMO}. Build and launch Air once first." >&2
  exit 1
fi

if [[ ! -f "${CONFIG_PATH}" ]]; then
  echo "Missing config at ${CONFIG_PATH}. Launch Air once first." >&2
  exit 1
fi

mkdir -p "$(dirname "${LOG_PATH}")"
rm -f "${PID_FILE}" "${LOG_PATH}"

run_admin_shell() {
  if [[ "${ADMIN_MODE}" == "sudo" ]]; then
    sudo -p "Air TUN verification needs administrator password: " /bin/sh -c "$1"
    return
  fi

  osascript - "$1" <<'APPLESCRIPT' &
on run argv
  do shell script item 1 of argv with administrator privileges with prompt "Air needs administrator privileges to verify mihomo TUN mode."
end run
APPLESCRIPT
  local osascript_pid=$!
  local waited=0
  while kill -0 "${osascript_pid}" 2>/dev/null; do
    if (( waited >= ADMIN_TIMEOUT_SECONDS )); then
      kill "${osascript_pid}" 2>/dev/null || true
      wait "${osascript_pid}" 2>/dev/null || true
      echo "Timed out waiting for macOS administrator authorization after ${ADMIN_TIMEOUT_SECONDS}s." >&2
      return 124
    fi
    sleep 1
    waited=$((waited + 1))
  done
  wait "${osascript_pid}"
}

shell_quote() {
  printf "'%s'" "${1//\'/\'\\\'\'}"
}

controller_from_config() {
  python3 - "${CONFIG_PATH}" "${CONTROLLER_FALLBACK}" <<'PY'
import sys
from pathlib import Path

try:
    import yaml
except Exception:
    yaml = None

config_path = Path(sys.argv[1])
fallback = sys.argv[2]

controller = fallback
if yaml is not None:
    data = yaml.safe_load(config_path.read_text()) or {}
    value = data.get("external-controller")
    if isinstance(value, str) and value.strip():
        controller = value.strip()
else:
    for line in config_path.read_text().splitlines():
        if line.startswith("external-controller:"):
            value = line.split(":", 1)[1].strip()
            if value:
                controller = value
            break

if controller.startswith(":"):
    controller = "127.0.0.1" + controller
elif controller.startswith("0.0.0.0:"):
    controller = "127.0.0.1:" + controller.rsplit(":", 1)[1]
elif controller.startswith("[::]:"):
    controller = "127.0.0.1:" + controller.rsplit(":", 1)[1]

print(controller)
PY
}

controller_port() {
  python3 - "$1" <<'PY'
import sys
from urllib.parse import urlparse

value = sys.argv[1].strip()
if "://" not in value:
    value = "http://" + value
parsed = urlparse(value)
if parsed.port is None:
    raise SystemExit("controller has no explicit port")
print(parsed.port)
PY
}

controller_url() {
  if [[ "$1" == http://* || "$1" == https://* ]]; then
    printf '%s\n' "$1"
  else
    printf 'http://%s\n' "$1"
  fi
}

existing_tun_default_routes() {
  netstat -rn -f inet | awk '$NF ~ /^utun/ && ($1 == "1" || $1 == "0/1" || $1 == "128.0/1" || $1 == "198.18.0/16") { print }'
}

echo "Validating mihomo config..."
"${MIHOMO}" -d "${CACHE_DIR}" -t -f "${CONFIG_PATH}"

CONTROLLER_ADDR="$(controller_from_config)"
CONTROLLER_PORT="$(controller_port "${CONTROLLER_ADDR}")"
CONTROLLER_URL="$(controller_url "${CONTROLLER_ADDR}")"

if [[ "${CHECK_ONLY}" == "1" ]]; then
  echo "macOS TUN verifier check-only preflight succeeded."
  exit 0
fi

if lsof -nP -iTCP:"${CONTROLLER_PORT}" -sTCP:LISTEN >/tmp/air-mihomo-controller-port.txt 2>/dev/null; then
  echo "Controller port ${CONTROLLER_PORT} is already in use; update ${CONFIG_PATH} before running full TUN verification." >&2
  cat /tmp/air-mihomo-controller-port.txt >&2
  exit 1
fi

if existing_tun_default_routes >/tmp/air-existing-tun-routes.txt && [[ -s /tmp/air-existing-tun-routes.txt ]]; then
  echo "Existing TUN default routes are already installed, likely by another proxy/VPN app. Disable that TUN/VPN before verifying Air." >&2
  cat /tmp/air-existing-tun-routes.txt >&2
  exit 1
fi

BEFORE_UTUN="$(ifconfig | awk '/^utun/{print $1}' | sort)"
SHELL_SCRIPT="cd $(shell_quote "${CACHE_DIR}"); $(shell_quote "${MIHOMO}") -d $(shell_quote "${CACHE_DIR}") -f $(shell_quote "${CONFIG_PATH}") >> $(shell_quote "${LOG_PATH}") 2>&1 & echo \$! > $(shell_quote "${PID_FILE}")"

echo "Starting mihomo with macOS administrator privileges..."
run_admin_shell "${SHELL_SCRIPT}"

if [[ ! -s "${PID_FILE}" ]]; then
  echo "mihomo did not write a PID file." >&2
  cat "${LOG_PATH}" >&2 || true
  exit 1
fi

PID="$(tr -d '[:space:]' < "${PID_FILE}")"
echo "mihomo PID: ${PID}"

cleanup() {
  if [[ -n "${PID:-}" ]] && ps -p "${PID}" >/dev/null 2>&1; then
    echo "Stopping mihomo..."
    run_admin_shell "kill -TERM ${PID} 2>/dev/null || true" || true
    sleep 2
    if ps -p "${PID}" >/dev/null 2>&1; then
      run_admin_shell "kill -KILL ${PID} 2>/dev/null || true" || true
    fi
  fi
  rm -f "${PID_FILE}"
}
trap cleanup EXIT

echo "Waiting for external-controller..."
for _ in {1..30}; do
  if curl -fsS "${CONTROLLER_URL}/version" >/tmp/air-mihomo-version.json 2>/dev/null; then
    break
  fi
  sleep 1
done

if [[ ! -s /tmp/air-mihomo-version.json ]]; then
  echo "external-controller did not become reachable." >&2
  cat "${LOG_PATH}" >&2 || true
  exit 1
fi

AFTER_UTUN="$(ifconfig | awk '/^utun/{print $1}' | sort)"
NEW_UTUN="$(comm -13 <(printf '%s\n' "${BEFORE_UTUN}") <(printf '%s\n' "${AFTER_UTUN}") | sed '/^$/d')"
if [[ -n "${NEW_UTUN}" ]]; then
  echo "Detected new utun interface:"
  echo "${NEW_UTUN}"
else
  echo "No new utun interface detected after startup." >&2
  cat "${LOG_PATH}" >&2 || true
  exit 1
fi

echo "Controller version:"
cat /tmp/air-mihomo-version.json
echo
echo "macOS TUN verification succeeded."
