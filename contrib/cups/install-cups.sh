#!/usr/bin/env bash
# Register thermorinterd as a CUPS virtual printer (Linux/macOS with CUPS).
#
# This makes the cat printer appear in the system Print… dialog. CUPS converts
# each job to PDF (per thermorinter.ppd) and hands it to the 'thermorinter'
# CUPS backend, which forwards it to a running thermorinterd over HTTP.
#
# Prereqs: thermorinterd running (e.g. `thermorinterd serve`), curl, CUPS.
# Usage: sudo ./contrib/cups/install-cups.sh [--name NAME] [--host 127.0.0.1:9100]
set -euo pipefail

NAME="thermorinter"
HOST="127.0.0.1:9100"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) NAME="$2"; shift 2;;
    --host) HOST="$2"; shift 2;;
    *) echo "unknown arg: $1"; exit 1;;
  esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"

# Locate the CUPS backend dir.
BACKEND_DIR="/usr/lib/cups/backend"
[[ -d "$BACKEND_DIR" ]] || BACKEND_DIR="/usr/libexec/cups/backend"  # macOS
if [[ ! -d "$BACKEND_DIR" ]]; then
  echo "Could not find the CUPS backend directory."; exit 1
fi

echo "==> Installing backend to $BACKEND_DIR/thermorinter"
sudo install -m 0755 -o root "$HERE/thermorinter" "$BACKEND_DIR/thermorinter"

echo "==> Creating/updating queue '$NAME' (device thermorinter://$HOST)"
sudo lpadmin -p "$NAME" -v "thermorinter://$HOST" -P "$HERE/thermorinter.ppd" -E
sudo lpadmin -p "$NAME" -o printer-is-shared=false

cat <<EOF

==> Done.
   Test:   echo hi | lp -d $NAME
           lp -d $NAME /path/to/document.pdf
   The daemon must be running:  thermorinterd serve --addr $HOST
   Remove: sudo lpadmin -x $NAME && sudo rm -f $BACKEND_DIR/thermorinter

Notes:
 - Linux/Raspberry Pi is the recommended host (no BLE permission friction).
 - On macOS, the daemon process needs the system Bluetooth permission grant.
EOF
