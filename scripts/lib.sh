#!/usr/bin/env bash
# Sourced by the scripts in this directory. Every variable is optional; precedence is the caller's
# environment, then $ENV_FILE, then the default below. The cargo variables let another workspace
# that builds on arcodash reuse these scripts with its own binaries.

DEFAULT_DATABASE_URL="postgres://arcodash:arcodash@localhost:5432/arcodash"
TEST_DATABASE_URL="${TEST_DATABASE_URL:-postgres://arcodash:arcodash@localhost:5433/arcodash_test}"
CARGO_MANIFEST_PATH="${CARGO_MANIFEST_PATH:-$ROOT/Cargo.toml}"
BACKEND_PACKAGE="${BACKEND_PACKAGE:-arcodash-backend}"
MIGRATE_PACKAGE="${MIGRATE_PACKAGE:-arcodash-migration}"
WORKER_PACKAGE="${WORKER_PACKAGE:-arcodash-worker}"
WORKER_BIN="${WORKER_BIN:-$WORKER_PACKAGE}"
ENV_FILE="${ENV_FILE:-$ROOT/.env}"

_cargo() {
  local sub="$1"
  shift
  cargo "$sub" --manifest-path "$CARGO_MANIFEST_PATH" "$@"
}

# Sets NAME from the last `NAME=` line of $ENV_FILE unless the caller already set it. Parses
# instead of sourcing, so unrelated .env values never leak into the environment.
_load_env_var() {
  local name="$1" line val
  [[ -n "${!name:-}" || ! -f "$ENV_FILE" ]] && return 0
  line=$(grep -E "^[[:space:]]*(export[[:space:]]+)?$name=" "$ENV_FILE" | tail -1) || true
  [[ -n "$line" ]] || return 0
  val="${line#*=}"
  if [[ "$val" =~ ^\"([^\"]*)\" || "$val" =~ ^\'([^\']*)\' ]]; then
    val="${BASH_REMATCH[1]}"
  else
    val="$(echo "$val" | sed -E 's/[[:space:]]+#.*$//; s/[[:space:]]+$//')"
  fi
  [[ -n "$val" ]] && export "$name=$val"
  return 0
}

# Sources $ENV_FILE with every variable exported; the caller's values for the named vars win.
_source_dotenv() {
  local -a keep=()
  local n
  for n in "$@"; do [[ -n "${!n:-}" ]] && keep+=("$n" "${!n}"); done
  set -a
  # shellcheck source=/dev/null
  source "$ENV_FILE"
  set +a
  while ((${#keep[@]})); do
    export "${keep[0]}=${keep[1]}"
    keep=("${keep[@]:2}")
  done
}

# The Docker-network Postgres host of .env.example, as seen from the host.
_localize_dsn() { echo "${1//postgres:5432/localhost:5432}"; }

_resolve_database_url() {
  _load_env_var DATABASE_URL
  DATABASE_URL="$(_localize_dsn "${DATABASE_URL:-$DEFAULT_DATABASE_URL}")"
  export DATABASE_URL
}

# Database name of a postgres:// DSN.
_db_name() {
  local path="${1#*://}"
  path="${path#*/}"
  echo "${path%%\?*}"
}

_url_host() {
  local url="$1" host
  [[ "$url" =~ ^[a-zA-Z]+://([^@/]*@)?(\[[^]]+\]|[^:/]+) ]] || return 1
  host="${BASH_REMATCH[2]}"
  echo "${host#[}" | sed 's/]$//'
}

_is_loopback_host() {
  case "$1" in
    localhost|127.0.0.1|::1) return 0 ;;
    *) return 1 ;;
  esac
}

# Exits unless <url> points at this machine. The scripts must never touch a remote deployment.
_assert_loopback() {
  local url="$1" host
  host=$(_url_host "$url") || { echo "==> cannot parse a host from: $url" >&2; exit 1; }
  _is_loopback_host "$host" || {
    echo "==> refusing: '$host' is not loopback; these scripts only run against localhost" >&2
    exit 1
  }
}

# Same for a Postgres DSN, including libpq's host=/hostaddr= query overrides.
_assert_local_dsn() {
  local dsn="$1" qs param
  _assert_loopback "$dsn"
  qs="${dsn#*\?}"
  [[ "$qs" == "$dsn" ]] && return 0
  for param in ${qs//&/ }; do
    case "${param%%=*}" in
      host|hostaddr) _is_loopback_host "${param#*=}" || {
        echo "==> refusing: DATABASE_URL parameter '$param' is not loopback" >&2
        exit 1
      } ;;
    esac
  done
}

# Loads $ENV_FILE for a native backend or worker, mapping the Docker hostnames in .env.example to
# localhost. The caller's values for the named vars win over .env.
_load_app_env() {
  [[ -f "$ENV_FILE" ]] || { echo "==> $ENV_FILE not found; run: cp .env.example .env" >&2; exit 1; }
  _source_dotenv "$@" BASE_URL DATABASE_URL VALKEY_URL SMTP_HOST STORAGE_EMULATOR_HOST
  _resolve_database_url
  _assert_local_dsn "$DATABASE_URL"
  export VALKEY_URL="${VALKEY_URL//valkey:6379/localhost:6379}"
  export SMTP_HOST="${SMTP_HOST//mailpit/localhost}"
  STORAGE_EMULATOR_HOST="${STORAGE_EMULATOR_HOST:-http://localhost:4443}"
  export STORAGE_EMULATOR_HOST="${STORAGE_EMULATOR_HOST//gcs:4443/localhost:4443}"
  export BASE_URL="${BASE_URL:-http://localhost:3000}"
}

# Whether something listens on TCP <port>. lsof on macOS; ss on Linux, where lsof is often missing;
# a plain connect when neither is installed.
_port_busy() {
  if command -v lsof >/dev/null; then
    lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1
  elif command -v ss >/dev/null; then
    ss -Hltn "sport = :$1" 2>/dev/null | grep -q .
  else
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
  fi
}

# Runs `<cmd...>` as service <name>: fg (exec in this terminal), start | restart (background in its
# own process group, so stop also reaches the children cargo or pnpm spawn), stop, status.
# Pid and log files: /tmp/arcodash-<name>.{pid,log}. Refuses to start if <port> is taken.
_service() {
  local name="$1" action="$2" port="$3" pid_file="/tmp/arcodash-$1.pid" log="/tmp/arcodash-$1.log" pid tries=20
  shift 3
  pid=$(cat "$pid_file" 2>/dev/null || true)
  case "$action" in
    status)
      if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then echo "$name running (pid $pid)"; else echo "$name not running"; fi
      return 0 ;;
    stop|restart)
      if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        echo "==> Stopping $name (pid $pid)"
        kill -- "-$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
        while ((tries--)) && kill -0 "$pid" 2>/dev/null; do sleep 0.5; done
      fi
      rm -f "$pid_file"
      [[ "$action" == stop ]] && return 0 ;;
  esac
  if _port_busy "$port"; then
    echo "==> port $port is already in use; stop that process or pick another port" >&2
    command -v lsof >/dev/null && lsof -nP -iTCP:"$port" -sTCP:LISTEN >&2
    exit 1
  fi
  if [[ "$action" == fg ]]; then exec "$@"; fi
  set -m
  # No terminal stdin: Vite reads it for shortcuts and crashes with EIO once backgrounded.
  "$@" < /dev/null > "$log" 2>&1 &
  set +m
  echo $! > "$pid_file"
  echo "    $name started (pid $!), log: $log"
}

# Creates the database named by <dsn> in <compose-file>'s <service> Postgres if missing. Never drops.
_ensure_db() {
  local file="$1" service="$2" name exists
  name=$(_db_name "$3")
  [[ "$name" =~ ^[A-Za-z0-9_]+$ ]] || { echo "==> refusing unsafe database name '$name'" >&2; return 1; }
  exists="$(docker compose -f "$file" exec -T "$service" psql -U arcodash -d postgres -tAc \
    "SELECT 1 FROM pg_database WHERE datname='$name'")"
  if [[ "$exists" != "1" ]]; then
    echo "==> Creating database $name"
    docker compose -f "$file" exec -T "$service" psql -U arcodash -d postgres -q \
      -c "CREATE DATABASE \"$name\" OWNER arcodash"
  fi
}
