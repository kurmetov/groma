#!/usr/bin/env bash
# Measure the reference corpus and compare against the accepted baseline in
# tests/baseline/corpus_metrics.tsv, failing on any movement the wrong way.
#
# This is the check to run before and after any change to
# the record walk. It replaces diffing two sweeps by eye: a class that is bought
# by selling another now fails instead of needing to be spotted.
#
#   scripts/corpus_check.sh            # measure and compare
#   scripts/corpus_check.sh --write    # accept the current numbers as baseline
#
#   RIVET_CORPUS=/path/to/rvt/files scripts/corpus_check.sh
set -euo pipefail

cd "$(dirname "$0")/.."

export RIVET_CORPUS=${RIVET_CORPUS:-data/test}

if [ ! -d "$RIVET_CORPUS" ]; then
  echo "no corpus at $RIVET_CORPUS - set RIVET_CORPUS to a directory of .rvt files" >&2
  exit 1
fi

# cargo runs a test from its package directory, so hand the test an absolute path.
RIVET_CORPUS=$(cd "$RIVET_CORPUS" && pwd)
export RIVET_CORPUS

if [ "${1:-}" = "--write" ]; then
  export RIVET_BASELINE_WRITE=1
  echo "accepting current measurements as the new baseline"
elif [ -n "${1:-}" ]; then
  echo "usage: $0 [--write]" >&2
  exit 2
fi

# The corpus runs to hundreds of megabytes a file; measure with the optimized
# build or the gate is slow enough that nobody runs it.
cargo build --release -p rivet-cli
export RIVET_BIN=${RIVET_BIN:-$PWD/target/release/rivet}

cargo test -p rivet-cli --test corpus_regression -- --nocapture
