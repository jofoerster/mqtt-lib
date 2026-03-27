#!/usr/bin/env bash
#
# Throughput benchmark for the WASI MQTT broker.
#
# Usage:
#   ./scripts/bench.sh [message_count] [payload_size] [broker_addr] [runs]
#
# Examples:
#   ./scripts/bench.sh                    # 1000 msgs, 64B, 5 runs
#   ./scripts/bench.sh 5000 256           # 5000 msgs, 256B, 5 runs
#   ./scripts/bench.sh 1000 64 127.0.0.1:1883 10   # 10 runs
#
# Prerequisites:
#   - mosquitto_pub / mosquitto_sub (apt install mosquitto-clients)
#   - Broker must already be running on the target address

set -euo pipefail

MSG_COUNT="${1:-1000}"
PAYLOAD_SIZE="${2:-64}"
BROKER="${3:-127.0.0.1:1883}"
RUNS="${4:-5}"
HOST="${BROKER%:*}"
PORT="${BROKER#*:}"
TOPIC="bench/throughput"
PAYLOAD=$(head -c "$PAYLOAD_SIZE" /dev/urandom | base64 | head -c "$PAYLOAD_SIZE")

# --- Statistics helpers ---
sort_array() { printf '%s\n' "$@" | sort -n; }

percentile() {
    local -n arr=$1
    local p=$2
    local n=${#arr[@]}
    local idx=$(( (n * p) / 100 ))
    [ "$idx" -ge "$n" ] && idx=$((n - 1))
    echo "${arr[$idx]}"
}

compute_stats() {
    local -n values=$1
    local label=$2
    local unit=$3
    local n=${#values[@]}

    local sum=0
    for v in "${values[@]}"; do sum=$((sum + v)); done
    local mean=$((sum / n))

    IFS=$'\n' sorted=($(sort_array "${values[@]}")); unset IFS
    local median="${sorted[$((n / 2))]}"
    local min="${sorted[0]}"
    local max="${sorted[$((n - 1))]}"
    local p95; p95=$(percentile sorted 95)

    # stddev (integer approximation)
    local sq_sum=0
    for v in "${values[@]}"; do
        local diff=$((v - mean))
        sq_sum=$((sq_sum + diff * diff))
    done
    local variance=$((sq_sum / n))
    # integer sqrt via Newton's method
    local stddev=0
    if [ "$variance" -gt 0 ]; then
        stddev=$variance
        local prev=0
        for _ in $(seq 1 20); do
            prev=$stddev
            stddev=$(( (stddev + variance / stddev) / 2 ))
            [ "$stddev" -eq "$prev" ] && break
        done
    fi

    printf "  %-12s  min=%-8s  median=%-8s  mean=%-8s  p95=%-8s  max=%-8s  stddev=%-8s\n" \
        "$label:" "${min}${unit}" "${median}${unit}" "${mean}${unit}" "${p95}${unit}" "${max}${unit}" "${stddev}${unit}"
}

echo "=== WASI MQTT Broker Benchmark ==="
echo "Broker:       $BROKER"
echo "Messages:     $MSG_COUNT"
echo "Payload size: $PAYLOAD_SIZE bytes"
echo "Runs:         $RUNS"
echo ""

# --- Latency ---
echo "--- Round-trip latency (single message, $RUNS runs) ---"
latencies=()
for r in $(seq 1 "$RUNS"); do
    mosquitto_sub -h "$HOST" -p "$PORT" -t "$TOPIC/lat" -C 1 -W 5 > /dev/null 2>&1 &
    SUB_PID=$!
    sleep 0.2

    START_NS=$(date +%s%N)
    mosquitto_pub -h "$HOST" -p "$PORT" -t "$TOPIC/lat" -m "ping"
    wait "$SUB_PID" 2>/dev/null || true
    END_NS=$(date +%s%N)

    ms=$(( (END_NS - START_NS) / 1000000 ))
    latencies+=("$ms")
done
compute_stats latencies "Latency" "ms"
echo ""

# --- Throughput ---
echo "--- Publish throughput ($MSG_COUNT messages, $RUNS runs) ---"
pub_rates=()
e2e_rates=()
throughputs=()
for r in $(seq 1 "$RUNS"); do
    RECV_FILE=$(mktemp)
    mosquitto_sub -h "$HOST" -p "$PORT" -t "$TOPIC/data" -C "$MSG_COUNT" -W 60 2>/dev/null | wc -l > "$RECV_FILE" &
    SUB_PID=$!
    sleep 0.3

    START=$(date +%s%N)
    for _ in $(seq 1 "$MSG_COUNT"); do
        mosquitto_pub -h "$HOST" -p "$PORT" -t "$TOPIC/data" -m "$PAYLOAD"
    done
    PUB_END=$(date +%s%N)

    wait "$SUB_PID" 2>/dev/null || true
    END=$(date +%s%N)

    RECEIVED=$(cat "$RECV_FILE")
    rm -f "$RECV_FILE"

    pub_ms=$(( (PUB_END - START) / 1000000 ))
    total_ms=$(( (END - START) / 1000000 ))

    [ "$pub_ms" -gt 0 ] && pub_rates+=("$(( MSG_COUNT * 1000 / pub_ms ))") || pub_rates+=(0)
    [ "$total_ms" -gt 0 ] && e2e_rates+=("$(( RECEIVED * 1000 / total_ms ))") || e2e_rates+=(0)
    [ "$total_ms" -gt 0 ] && throughputs+=("$(( RECEIVED * PAYLOAD_SIZE * 1000 / total_ms / 1024 ))") || throughputs+=(0)
done
compute_stats pub_rates "Pub rate" "msg/s"
compute_stats e2e_rates "E2E rate" "msg/s"
compute_stats throughputs "Throughput" "KB/s"
echo ""

# --- Fan-out ---
FANOUT_COUNT=$((MSG_COUNT / 10))
echo "--- Fan-out (1 pub -> 5 subs, $FANOUT_COUNT messages, $RUNS runs) ---"
fanout_times=()
fanout_recv=()
for r in $(seq 1 "$RUNS"); do
    PIDS=()
    for i in $(seq 1 5); do
        RF="/tmp/bench_fan_${r}_${i}"
        mosquitto_sub -h "$HOST" -p "$PORT" -t "$TOPIC/fan" -C "$FANOUT_COUNT" -W 60 2>/dev/null | wc -l > "$RF" &
        PIDS+=($!)
    done
    sleep 0.5

    START=$(date +%s%N)
    for _ in $(seq 1 "$FANOUT_COUNT"); do
        mosquitto_pub -h "$HOST" -p "$PORT" -t "$TOPIC/fan" -m "$PAYLOAD"
    done

    for pid in "${PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
    END=$(date +%s%N)

    total_ms=$(( (END - START) / 1000000 ))
    total_recv=0
    for i in $(seq 1 5); do
        RF="/tmp/bench_fan_${r}_${i}"
        count=$(cat "$RF" 2>/dev/null || echo 0)
        total_recv=$((total_recv + count))
        rm -f "$RF"
    done

    fanout_times+=("$total_ms")
    fanout_recv+=("$total_recv")
done
compute_stats fanout_times "Duration" "ms"
compute_stats fanout_recv "Received" "msgs"
echo "  Expected:     $((FANOUT_COUNT * 5)) messages per run"
echo ""
echo "=== Done ==="
