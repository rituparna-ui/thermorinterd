#!/usr/bin/env bash
# Install thermorinterd as a service on Linux (systemd) or macOS (launchd).
# Builds the release binary, installs it to /usr/local/bin, and registers the
# service. Re-run to update. Usage: ./contrib/install.sh [--device NAME] [--addr H:P]
set -euo pipefail

DEVICE=""
ADDR="127.0.0.1:9100"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --device) DEVICE="$2"; shift 2;;
    --addr)   ADDR="$2"; shift 2;;
    *) echo "unknown arg: $1"; exit 1;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> Building release binary"
cargo build --release
BIN="$ROOT/target/release/thermorinterd"

echo "==> Installing to /usr/local/bin (may prompt for sudo)"
sudo install -m 0755 "$BIN" /usr/local/bin/thermorinterd

ARGS="serve --addr $ADDR"
[[ -n "$DEVICE" ]] && ARGS="$ARGS --device $DEVICE"

OS="$(uname -s)"
if [[ "$OS" == "Linux" ]]; then
  echo "==> Installing systemd unit"
  TMP="$(mktemp)"
  sed "s#ExecStart=.*#ExecStart=/usr/local/bin/thermorinterd $ARGS#" \
    "$ROOT/contrib/systemd/thermorinterd.service" > "$TMP"
  sudo install -m 0644 "$TMP" /etc/systemd/system/thermorinterd.service
  rm -f "$TMP"
  sudo systemctl daemon-reload
  sudo systemctl enable --now thermorinterd
  echo "==> Done. Status: sudo systemctl status thermorinterd"
elif [[ "$OS" == "Darwin" ]]; then
  echo "==> Installing launchd LaunchAgent (runs as $USER)"
  PLIST="$HOME/Library/LaunchAgents/dev.thermorinter.daemon.plist"
  mkdir -p "$HOME/Library/LaunchAgents"
  python3 - "$ADDR" "$DEVICE" > "$PLIST" <<'PY'
import sys, plistlib
addr, device = sys.argv[1], sys.argv[2]
args = ["/usr/local/bin/thermorinterd", "serve", "--addr", addr]
if device:
    args += ["--device", device]
d = {
    "Label": "dev.thermorinter.daemon",
    "ProgramArguments": args,
    "RunAtLoad": True,
    "KeepAlive": {"SuccessfulExit": False},
    "StandardOutPath": "/tmp/thermorinterd.out.log",
    "StandardErrorPath": "/tmp/thermorinterd.err.log",
}
sys.stdout.buffer.write(plistlib.dumps(d))
PY
  launchctl unload "$PLIST" 2>/dev/null || true
  launchctl load -w "$PLIST"
  echo "==> Done. Loaded $PLIST"
  echo "    NOTE: grant Bluetooth permission to the launching process if prompted."
else
  echo "Unsupported OS: $OS (run 'thermorinterd serve' manually)"; exit 1
fi
