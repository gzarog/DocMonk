#!/usr/bin/env bash
# P0 baseline/benchmark harness (ADR 0033).
#
# Generates the deterministic multi-repository corpus, then measures, in a
# fresh DOCMONK_HOME each:
#   cold        first full index of every source
#   warm        unchanged re-index
#   edit        one file edited in one source, re-index of everything
#   query       search / symbol / explore wall time
#   failed      one source made unreachable, re-index of everything
# and (with DOCMONK_P0_SERVER_URL) the same in server mode.
#
# usage: scripts/p0-baseline.sh BIN OUT.json [SOURCES] [FILES] [ENGINE]
#   BIN     the docmonk binary to measure
#   SOURCES default 20, FILES default 4000 (use 150 / 200000 for the full run)
#   ENGINE  opensearch|elasticsearch for server mode (needs DOCMONK_P0_SERVER_URL)
set -euo pipefail

BIN=$(realpath "$1")
OUT=$2
SOURCES=${3:-20}
FILES=${4:-4000}
ENGINE=${5:-opensearch}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
ROOT=$(cd "$(dirname "$0")/.." && pwd)

export DOCMONK_UPDATES__ENABLED=false
ms() { date +%s%3N; }
elapsed() { local s; s=$(ms); "$@" >/dev/null 2>"$WORK/last.err" || echo "exit=$?" >>"$WORK/last.err"; echo $(( $(ms) - s )); }

(cd "$ROOT" && cargo xtask p0-corpus --out "$WORK/corpus" --sources "$SOURCES" --files "$FILES" --with-documents >/dev/null)

run_mode() {
  local mode=$1
  export DOCMONK_HOME="$WORK/home-$mode"
  rm -rf "$DOCMONK_HOME"
  if [ "$mode" = server ]; then
    "$BIN" init --storage-mode server --storage-engine "$ENGINE" \
      --storage-url "$DOCMONK_P0_SERVER_URL" --storage-index-prefix "p0bench$$" >/dev/null
    "$BIN" server init >/dev/null 2>&1 || true
  else
    "$BIN" init >/dev/null
  fi
  for d in "$WORK"/corpus/repo-*; do "$BIN" source add "$d" >/dev/null; done
  local cold warm edit q_search q_symbol q_explore failed status_ok
  cold=$(elapsed "$BIN" index)
  warm=$(elapsed "$BIN" index)
  local f
  f=$(find "$WORK/corpus/repo-001/src" -name '*.cs' | sort | head -1)
  printf '\n// edited %s\n' "$(ms)" >>"$f"
  edit=$(elapsed "$BIN" index)
  q_search=$(elapsed "$BIN" search "settlement ledger" --json)
  q_symbol=$(elapsed "$BIN" symbol "$(basename "$f" .cs)" --json)
  q_explore=$(elapsed "$BIN" explore "who calls the billing service" --json)
  mv "$WORK/corpus/repo-002" "$WORK/corpus/repo-002.off"
  failed=$(elapsed "$BIN" index)
  mv "$WORK/corpus/repo-002.off" "$WORK/corpus/repo-002"
  if "$BIN" status --json >"$WORK/status-$mode.json" 2>/dev/null; then status_ok=true; else status_ok=false; fi
  local backend
  backend=$(jq -c '.backend // null' "$WORK/status-$mode.json" 2>/dev/null || echo null)
  local local_dbs
  local_dbs=$(find "$DOCMONK_HOME/projects" -name knowledge.db 2>/dev/null | wc -l)
  local hits
  hits=$("$BIN" search "settlement ledger" --json 2>/dev/null | grep -c '"path"' || true)
  printf '{"mode":"%s","cold_ms":%s,"warm_ms":%s,"edit_one_file_ms":%s,"query_ms":{"search":%s,"symbol":%s,"explore":%s},"failed_source_ms":%s,"status_ok":%s,"status_backend":%s,"local_knowledge_dbs":%s,"search_result_paths":%s}' \
    "$mode" "$cold" "$warm" "$edit" "$q_search" "$q_symbol" "$q_explore" "$failed" "$status_ok" "${backend:-null}" "$local_dbs" "${hits:-0}"
}

LOCAL=$(run_mode local)
SERVER=null
if [ -n "${DOCMONK_P0_SERVER_URL:-}" ]; then SERVER=$(run_mode server); fi

cat >"$OUT" <<EOF
{
  "harness": "scripts/p0-baseline.sh",
  "binary": "$("$BIN" version 2>/dev/null | head -1)",
  "corpus": {"generator": "cargo xtask p0-corpus --with-documents", "sources": $SOURCES, "files": $FILES, "seed": 1},
  "host": {"cpus": $(nproc), "mem_gb": $(free -g | awk '/Mem:/{print $2}'), "cpu_model": "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"},
  "local": $LOCAL,
  "server": $SERVER
}
EOF
echo "wrote $OUT"
