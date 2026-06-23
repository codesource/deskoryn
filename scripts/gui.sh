#!/usr/bin/env bash
# GUI test helper for the iced tray UI (deskoryn-ui).
#
# Wraps the launch / click / screenshot / pairing steps behind one stable
# command so the screenshot-driven validation loop can run without a permission
# prompt per call (allowlist `Bash(bash scripts/gui.sh:*)`).
#
# Usage:
#   bash scripts/gui.sh launch              # kill old + launch UI on $DISPLAY
#   bash scripts/gui.sh win                 # print the window id
#   bash scripts/gui.sh click X Y           # click at window-relative X,Y
#   bash scripts/gui.sh shot FILE           # screenshot the window to FILE
#   bash scripts/gui.sh status              # `deskorynd status`
#   bash scripts/gui.sh port                # bound port from status
#   bash scripts/gui.sh dial-pair PORT      # dial 127.0.0.1:PORT as a separate
#                                           #   identity, auto-confirm the SAS
#   bash scripts/gui.sh kill                # stop UI + daemon
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export DISPLAY="${DISPLAY:-:1}"
# The UI is its own workspace (builds into ui/target); the daemon is the root workspace.
UI="$ROOT/ui/target/debug/deskoryn-ui"
DAEMON="$ROOT/target/release/deskorynd"

win() { xdotool search --name "^Deskoryn$" 2>/dev/null | tail -1; }
# Real sleep (seconds). xrefresh-style "settles" returned instantly; use sleep.
settle() { sleep "${1:-1}"; }

cmd="${1:-}"; shift || true
case "$cmd" in
  launch)
    pkill -x deskoryn-ui 2>/dev/null || true
    # Detach so the UI survives this one-shot shell (setsid = new session).
    setsid "$UI" >/tmp/deskoryn-ui.log 2>&1 < /dev/null &
    for _ in $(seq 1 30); do [ -n "$(win)" ] && break; sleep 0.5; done
    echo "win=$(win)"
    ;;
  win) win ;;
  wait) settle "${1:-4}" ;;
  click)
    W="$(win)"; xdotool windowactivate --sync "$W" 2>/dev/null
    xdotool mousemove --window "$W" "$1" "$2" click 1
    settle 2
    ;;
  shot)
    W="$(win)"; settle 1
    import -window "$W" "$1" 2>/dev/null && echo "saved $1"
    ;;
  status) "$DAEMON" status 2>&1 ;;
  port) "$DAEMON" status 2>&1 | grep -oE 'port:[[:space:]]+[0-9]+' | grep -oE '[0-9]+' ;;
  dial-pair)
    XDG_DATA_HOME=/tmp/dskdial-data XDG_CONFIG_HOME=/tmp/dskdial-cfg \
      printf 'y\n' | XDG_DATA_HOME=/tmp/dskdial-data XDG_CONFIG_HOME=/tmp/dskdial-cfg \
      "$DAEMON" pair "127.0.0.1:$1" >/tmp/deskoryn-dial.log 2>&1 &
    echo "dialer started (log: /tmp/deskoryn-dial.log)"
    ;;
  kill) pkill -x deskoryn-ui 2>/dev/null || true; pkill -x deskorynd 2>/dev/null || true; echo killed ;;
  *) echo "usage: gui.sh {launch|win|click X Y|shot FILE|status|port|dial-pair PORT|kill}" >&2; exit 2 ;;
esac
