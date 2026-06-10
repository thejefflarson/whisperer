#!/usr/bin/env bash
# Model-check the TLA+ spec of the reconcile loop (docs/model/Whisper.tla).
#
# Runs both configurations through TLC: the safety spec (invariants) and the
# sync-only spec (the Convergence liveness property). Downloads tla2tools.jar into
# a cache dir on first run.
#
# Requires: java. Usage: scripts/check-model.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL="$ROOT/docs/model"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/whisperer"
JAR="$CACHE/tla2tools.jar"
JAR_URL="https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar"

command -v java >/dev/null 2>&1 || {
  echo "error: 'java' not found on PATH (needed to run TLC)" >&2
  exit 1
}

if [[ ! -f "$JAR" ]]; then
  echo "==> fetching tla2tools.jar -> $JAR"
  mkdir -p "$CACHE"
  curl -fsSL -o "$JAR" "$JAR_URL"
fi

# Run TLC from the model dir so it finds the .tla/.cfg files; -cleanup removes the
# states/ checkpoint dir TLC leaves behind.
cd "$MODEL"
run() {
  local cfg="$1" tla="$2"
  echo "==> TLC: $cfg"
  java -cp "$JAR" tlc2.TLC -cleanup -config "$cfg" "$tla"
}

run Whisper.cfg Whisper.tla
run WhisperLiveness.cfg Whisper.tla

echo "==> model check passed"
