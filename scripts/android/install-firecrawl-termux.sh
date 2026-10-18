#!/data/data/com.termux/files/usr/bin/env bash
# Installs a rootless, ARM64 Firecrawl deployment inside Termux for GnomeAI.
# No Podman/Docker: the API runs from the official image via Podman only on
# desktop, so here we run the components directly:
#   redis-server  (queue + rate limiting)
#   firecrawl API (Node 20+, from the firecrawl source tree)
#   playwright-service is NOT installed: Android/Termux has no headless Chromium;
#   scrapes fall back to the HTTP fetcher inside the API.
#
# Usage:
#   install-firecrawl-termux.sh            # install + ensure services
#   install-firecrawl-termux.sh logs       # tail the API log
#   install-firecrawl-termux.sh stop       # stop all services
set -euo pipefail

PREFIX=/data/data/com.termux/files/usr
ROOT=/data/data/com.termux/files/home/.termux/firecrawl
API_DIR=$ROOT/firecrawl-api
LOG_DIR=$ROOT/logs
RUN_DIR=$ROOT/run
API_PORT=3002
REDIS_PORT=6397

log() { printf '[gnomeai-firecrawl] %s\n' "$*"; }

die() { printf '[gnomeai-firecrawl] ERROR: %s\n' "$*" >&2; exit 1; }

ensure_pkg() {
    command -v "$1" >/dev/null 2>&1 && return 0
    log "installing package $2"
    pkg install -y "$2" >/dev/null 2>&1 || die "cannot install $2"
}

ensure_prereqs() {
    command -v node >/dev/null 2>&1 || die "Node.js is required: pkg install nodejs"
    ensure_pkg redis-server redis
    ensure_pkg curl curl
    mkdir -p "$LOG_DIR" "$RUN_DIR"
    install_self
}

# The app resolves the launcher at ~/.termux/firecrawl/gnomeai-firecrawl;
# keep that copy in sync with this file on every ensure.
install_self() {
    self=$ROOT/gnomeai-firecrawl
    if [ ! -e "$self" ] || ! cmp -s "$0" "$self"; then
        cp -f "$0" "$self"
        chmod +x "$self"
    fi
}

api_up() {
    curl --fail --silent --max-time 2 "http://127.0.0.1:$API_PORT/" >/dev/null 2>&1
}

redis_up() {
    redis-cli -p "$REDIS_PORT" ping >/dev/null 2>&1
}

start_redis() {
    redis_up && return 0
    log "starting redis on :$REDIS_PORT"
    redis-server --port "$REDIS_PORT" --daemonize yes \
        --dir "$RUN_DIR" --save '' --appendonly no
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        redis_up && return 0
        sleep 1
    done
    die "redis did not become ready on :$REDIS_PORT"
}

fetch_api_source() {
    [ -d "$API_DIR" ] && return 0
    log "cloning the firecrawl API source (first run only)"
    pkg install -y git >/dev/null 2>&1 || true
    git clone --depth 1 --recurse-submodules --shallow-submodules \
        https://github.com/firecrawl/firecrawl.git "$API_DIR" \
        || die "cannot clone the firecrawl repository"
}

install_api() {
    [ -f "$API_DIR/apps/api/dist/src/index.js" ] && return 0
    log "installing API dependencies (one-time, several minutes)"
    # tsc comes from devDependencies, so a plain dev install is required.
    (cd "$API_DIR/apps/api" && npm install --no-audit --no-fund) \
        || die "npm install failed"
    (cd "$API_DIR/apps/api" && npx tsc) || die "API build failed"
    (cd "$API_DIR/apps/api" && npm prune --omit=dev) || true
}

start_api() {
    api_up && return 0
    log "starting firecrawl API on :$API_PORT"
    (
        cd "$API_DIR/apps/api"
        PORT=$API_PORT HOST=127.0.0.1 \
        REDIS_URL="redis://127.0.0.1:$REDIS_PORT" \
        REDIS_RATE_LIMIT_URL="redis://127.0.0.1:$REDIS_PORT" \
        USE_DB_AUTHENTICATION=false \
        TEST_API_KEY=fc-local-android \
        NUM_WORKERS_PER_QUEUE=2 \
        MAX_CONCURRENT_JOBS=1 \
        CRAWL_CONCURRENT_REQUESTS=2 \
        BROWSER_POOL_SIZE=1 \
        PLAYWRIGHT_MICROSERVICE_URL=http://127.0.0.1:1/scrape \
        LOGGING_LEVEL=warn \
        nohup node dist/src/index.js >>"$LOG_DIR/api.log" 2>&1 &
        echo $! >"$RUN_DIR/api.pid"
    )
    for _ in $(seq 1 90); do
        api_up && { log "firecrawl API is ready on :$API_PORT"; return 0; }
        sleep 1
    done
    die "API did not become ready; see $LOG_DIR/api.log"
}

case "${1:-ensure}" in
    ensure)
        ensure_prereqs
        start_redis
        fetch_api_source
        install_api
        start_api
        ;;
    logs)
        tail -n 100 "$LOG_DIR/api.log" 2>/dev/null || log "no log yet"
        ;;
    stop)
        redis-cli -p "$REDIS_PORT" shutdown nosave >/dev/null 2>&1 || true
        [ -f "$RUN_DIR/api.pid" ] && kill "$(cat "$RUN_DIR/api.pid")" >/dev/null 2>&1 || true
        rm -f "$RUN_DIR/api.pid"
        log "stopped"
        ;;
    status)
        if api_up; then log "API: up (:${API_PORT})"; else log "API: down"; fi
        if redis_up; then log "redis: up (:${REDIS_PORT})"; else log "redis: down"; fi
        ;;
    *)
        die "usage: $0 [ensure|logs|stop|status]"
        ;;
esac
