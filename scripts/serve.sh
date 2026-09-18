#!/usr/bin/env bash
# Build and run the viewer locally, without Docker.
#
#   scripts/serve.sh                 # release build, port 8800
#   scripts/serve.sh 9000            # another port
#   SCENES=/path/to/scenes scripts/serve.sh
#
# Stop it with Ctrl-C. Scenes are served from $SCENES (default data/scenes);
# anything uploaded through the page is converted into the same directory.
set -euo pipefail

cd "$(dirname "$0")/.."

PORT=${1:-8800}
SCENES=${SCENES:-data/scenes}
MODELS=${MODELS:-data/api}

mkdir -p "$SCENES" "$MODELS"

echo "building (release, so a big model is not decoded by a debug build)..."
cargo build --release -p openrvt-api -p openrvt-cli

# Refuse to start on a port already in use rather than failing obscurely.
if ss -ltn "sport = :$PORT" 2>/dev/null | grep -q LISTEN; then
  echo "port $PORT is already in use - pass another, or stop what is on it" >&2
  exit 1
fi

echo
exec ./target/release/openrvt-api \
  --openrvt "$PWD/target/release/openrvt" \
  --data "$MODELS" \
  --scenes "$SCENES" \
  --addr "127.0.0.1:$PORT"
