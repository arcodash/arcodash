#!/usr/bin/env bash
# Lists or stops what the local scripts started: this checkout's backend, worker and frontend
# processes, and the arcodash* containers. Never deletes data (containers are only stopped).
# Usage: scripts/local.sh [status|stop] [--procs|--dbs]
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./lib.sh
source "$ROOT/scripts/lib.sh"
PID_FILES=(/tmp/arcodash-backend.pid /tmp/arcodash-worker.pid /tmp/arcodash-frontend.pid)
ACTION=status PROCS=false DBS=false
for arg in "$@"; do
  case "$arg" in
    status|stop) ACTION="$arg" ;;
    --procs) PROCS=true ;;
    --dbs) DBS=true ;;
    *) echo "Usage: $0 [status|stop] [--procs|--dbs]" >&2; exit 1 ;;
  esac
done
$PROCS || $DBS || { PROCS=true; DBS=true; }

# A process is ours if its working directory, or (for a workspace built elsewhere) its command
# line, points into this checkout. Other projects' servers are never touched.
_is_ours() {
  local cwd dir="${CARGO_MANIFEST_PATH%/*}"
  cwd=$(readlink "/proc/$1/cwd" 2>/dev/null || lsof -a -d cwd -p "$1" -Fn 2>/dev/null | sed -n 's/^n//p')
  [[ "$cwd" == "$ROOT" || "$cwd" == "$ROOT"/* ]] && return 0
  [[ "$dir" != "$ROOT" && "$(ps -p "$1" -ww -o command= 2>/dev/null)" == *"$dir/"* ]]
}

# Pid-file entries plus any of this checkout's backend, worker or Vite processes started by hand;
# both go through _is_ours so a stale pid file never points at a reused pid.
_pids() {
  { cat "${PID_FILES[@]}" 2>/dev/null
    pgrep -f "target/debug/($BACKEND_PACKAGE|$WORKER_BIN)|vite" 2>/dev/null; } | while read -r pid; do
    _is_ours "$pid" && echo "$pid"
  done
}

# Signals the whole process group when <pid> leads one (the scripts start services that way).
_signal() {
  if [[ "$(ps -o pgid= -p "$1" 2>/dev/null | tr -d ' ')" == "$1" ]]; then
    kill "-$2" -- "-$1" 2>/dev/null
  else
    kill "-$2" "$1" 2>/dev/null
  fi
}

PIDS=$(_pids | sort -un)
if $PROCS; then
  echo "== Processes =="
  for pid in $PIDS; do
    kill -0 "$pid" 2>/dev/null || continue
    if [[ "$ACTION" == stop ]]; then
      echo "  stopping $pid $(ps -p "$pid" -o comm= 2>/dev/null)"
      _signal "$pid" TERM
    else
      echo "  $pid $(ps -p "$pid" -ww -o command= 2>/dev/null)"
    fi
  done
  if [[ "$ACTION" == stop ]]; then
    sleep 1
    for pid in $PIDS; do kill -0 "$pid" 2>/dev/null && _signal "$pid" KILL; done
    rm -f "${PID_FILES[@]}"
  fi
fi

if $DBS; then
  echo "== Containers =="
  if [[ "$ACTION" == stop ]]; then
    ids=$(docker ps -q --filter 'name=^arcodash' 2>/dev/null)
    # shellcheck disable=SC2086
    [[ -n "$ids" ]] && docker stop $ids >/dev/null && echo "  stopped"
  else
    docker ps --filter 'name=^arcodash' --format '  {{.Names}}\t{{.Status}}\t{{.Ports}}' 2>/dev/null
  fi
fi
echo "Data is kept. To wipe the dev databases: scripts/infra.sh reset."
exit 0
