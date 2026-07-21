#!/usr/bin/env bash
# Claim-3 evidence harness (SPEC.md "Design targets", claims.md claim 3).
#
# Drives sustained concurrent ingest (via the Rust loadgen, keep-alive
# connections) with a query racing alongside, then reports events/sec and peak
# RSS. NOT the published number — that requires the stated 1 vCPU / 1 GB box.
# This is a real local number to track against targets and catch regressions.
#
# Usage: scripts/loadtest.sh [total_events] [threads]
set -euo pipefail

TOTAL="${1:-100000}"
THREADS="${2:-8}"
PORT=18930
DATA="$(mktemp -d)"
BIN="./target/release/hoglet"

echo "building release binary + loadgen..."
cargo build --release >/dev/null 2>&1
cargo build --release --example loadgen >/dev/null 2>&1

echo "load test: $TOTAL events, $THREADS keep-alive connections"
HOGLET_ADDR="127.0.0.1:$PORT" HOGLET_DATA="$DATA" "$BIN" >/dev/null 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true; rm -rf "$DATA"' EXIT
until curl -sf "http://127.0.0.1:$PORT/ready" >/dev/null 2>&1; do sleep 0.2; done

# Sample RSS (KB on both macOS and Linux ps) while the run proceeds.
( while kill -0 "$PID" 2>/dev/null; do ps -o rss= -p "$PID" 2>/dev/null | tr -d ' '; sleep 0.2; done ) > "$DATA/rss.log" &
SAMPLER=$!

# A funnel/stats query races the ingest to exercise both lanes at once.
( for _ in $(seq 1 40); do curl -s -o /dev/null "http://127.0.0.1:$PORT/api/stats?token=phc_load" || true; sleep 0.25; done ) &
QUERY=$!

RESULT="$(./target/release/examples/loadgen "$PORT" "$TOTAL" "$THREADS")"

kill "$SAMPLER" "$QUERY" 2>/dev/null || true
PEAK_KB="$(sort -n "$DATA/rss.log" | tail -1)"
PEAK_MB="$(echo "scale=1; ${PEAK_KB:-0} / 1024" | bc)"

echo "----------------------------------------"
echo "$RESULT"
echo "peak RSS:      ${PEAK_MB} MB"
echo "----------------------------------------"
echo "targets (SPEC.md): >= 5000 events/s, RSS < 400 MB (on 1 vCPU / 1 GB)"
