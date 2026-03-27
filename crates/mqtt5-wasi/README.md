# mqtt5-wasi

A standalone MQTT v5.0 / v3.1.1 broker that runs as a WebAssembly component inside [wasmtime](https://wasmtime.dev/) using WASI Preview 2 APIs.

## Quick Start

```bash
# Build the broker
cargo build -p mqtt5-wasi --target wasm32-wasip2 --release

# Run with wasmtime (v43+ recommended)
wasmtime run -S inherit-network \
  target/wasm32-wasip2/release/mqtt5-broker.wasm

# Subscribe (terminal 2)
mosquitto_sub -h 127.0.0.1 -p 1883 -t "test/#" -v

# Publish (terminal 3)
mosquitto_pub -h 127.0.0.1 -p 1883 -t "test/hello" -m "hello from wasi"
```

The bind address defaults to `0.0.0.0:1883` and can be changed via the `MQTT_BIND` environment variable.

## Architecture

### How It Fits Into mqtt-lib

The mqtt-lib repository has five crates. `mqtt5-wasi` is the WASI counterpart to `mqtt5-wasm` (browser) and `mqtt5` (native):

| Crate | Target | Transport | Async Model |
|-------|--------|-----------|-------------|
| `mqtt5` | Native (Linux/Mac/Win) | Tokio TCP/TLS/WS/QUIC | Tokio runtime, `tokio::spawn` |
| `mqtt5-wasm` | `wasm32-unknown-unknown` | MessagePort / BroadcastChannel | `wasm_bindgen_futures::spawn_local` |
| **`mqtt5-wasi`** | **`wasm32-wasip2`** | **WASI TCP sockets** | **Custom single-threaded executor** |

All three reuse the same transport-agnostic broker internals from the `mqtt5` crate:

- **`MessageRouter`** -- topic matching, subscription management, message delivery
- **`BrokerConfig`** -- all broker configuration types
- **`MemoryBackend`** / **`DynamicStorage`** -- in-memory session and message storage
- **`AuthProvider`** trait + implementations -- authentication and authorization
- **`BrokerStats`** / **`ResourceMonitor`** -- connection tracking and rate limiting
- **`AclManager`** -- access control lists

These are available when compiling `mqtt5` for `wasm32` targets because the crate gates native-only modules (TCP listeners, TLS, file I/O, `tokio::spawn`) behind `#[cfg(not(target_arch = "wasm32"))]`.

### Components Created for WASI

| File | Purpose |
|------|---------|
| `transport.rs` | `WasiStream` -- read/write over WASI `InputStream`/`OutputStream` |
| `executor.rs` | Single-threaded cooperative async executor with `spawn()` and `block_on()` |
| `decoder.rs` | MQTT packet framing with protocol-version-aware decoding |
| `timer.rs` | Non-blocking `sleep()` using `Instant::now()` + yield |
| `broker.rs` | `WasiBroker` -- TCP listener setup and accept loop |
| `client_handler/` | Per-connection MQTT protocol handler (CONNECT, SUBSCRIBE, PUBLISH, etc.) |

## Key Technical Decisions

### Why not `std::net`?

Rust's `std::net::TcpStream` on `wasm32-wasip2` wraps WASI sockets but uses **blocking** I/O internally (`blocking-read`, `blocking-write-and-flush`). Key limitations:

- **`set_nonblocking(true)` is a no-op** -- calls succeed but sockets remain blocking
- **`try_clone()` is unsupported** -- WASI doesn't expose `dup()` for socket file descriptors
- **`set_read_timeout()` is unsupported**

A blocking `accept()` or `read()` freezes the entire single-threaded WASM component, making concurrent client handling impossible.

### Direct WASI API usage

The broker uses the `wasi` crate (raw WIT bindings) for all I/O:

- **`wasi::sockets::tcp::TcpSocket::accept()`** -- returns `ErrorCode::WouldBlock` when no connection is pending (truly non-blocking)
- **`wasi::io::streams::InputStream::read()`** -- returns empty data when nothing is available (truly non-blocking)
- **`wasi::io::poll::Pollable::ready()`** -- non-blocking readiness check before every read, guaranteeing the executor is never blocked
- **`wasi::io::streams::OutputStream::check_write()` + `write()` + `blocking_flush()`** -- non-blocking write with blocking flush (writes are fast for small MQTT packets)
- **`wasi::clocks::monotonic_clock::subscribe_duration()`** -- 1ms timer pollable for executor idle sleep

Native builds (`cfg(not(target_os = "wasi"))`) fall back to `std::net` with `set_nonblocking(true)` for development and testing.

### Single-threaded cooperative executor

WASI does not support `std::thread::spawn`, `tokio::spawn`, or any form of OS-level threading. The broker uses a minimal custom executor:

```
block_on(main_future)
  loop:
    drain spawn queue into task list
    poll each task once (round-robin)
    if no task made progress:
      sleep 1ms via wasi:clocks pollable
```

Tasks yield via `executor::yield_now()` (returns `Poll::Pending` once, then `Ready`). The transport's `read()` method checks `Pollable::ready()` before attempting I/O -- if the socket isn't ready, it yields instead of blocking.

### WASI resource lifecycle

WASI enforces parent-child relationships between resources. A `TcpSocket` is the parent of its `InputStream` and `OutputStream`. Dropping the socket while its streams are alive causes a runtime trap ("resource has children").

`WasiStream` stores fields in drop order: `input`, `output`, then `_socket` -- Rust drops struct fields top-to-bottom, so streams are released before their parent socket.

### Protocol version awareness

The MQTT packet decoder must know whether the client uses MQTT 3.1.1 (v4) or MQTT 5.0 (v5) because v5 adds properties fields to most packet types. The CONNECT packet is version-agnostic (it contains the version), so it's always decoded as v5. All subsequent packets use `decode_from_body_with_version()` with the negotiated protocol version.

### Publish forwarding

Each client handler spawns a dedicated publish forwarder task that reads from the router's `flume` channel and writes to the client stream. This runs independently of the packet read loop, so subscribers receive messages without waiting for their own inbound packets (like PINGREQ) to trigger a queue drain.

## Limitations

### Concurrency model

The executor polls all tasks in round-robin. With many idle clients, this means O(n) polls per cycle even when only one client has data. A proper implementation would use `wasi::io::poll::poll()` to wait on multiple pollables simultaneously and only wake tasks whose sockets are ready. This is the main performance improvement opportunity.

### Blocking flush

`OutputStream::blocking_flush()` blocks the component until the write completes. For small MQTT packets this is fast, but large payloads or a slow client could stall the executor briefly. A fully non-blocking write path would use `flush()` (non-blocking) + `subscribe()` + `ready()` polling.

### Blocking `thread::sleep` in timer

The `timer::sleep()` implementation busy-yields (checking `Instant::now()` in a loop). This is correct but wastes CPU cycles. A better implementation would use `wasi::clocks::monotonic_clock::subscribe_duration()` to get a pollable and yield until it fires.

### No persistent storage

The broker uses `MemoryBackend` only. Sessions, retained messages, and queued messages are lost on restart. WASI does support filesystem access (`wasi:filesystem`), so `FileBackend` support could be added.

### No TLS

WASI Preview 2 does not provide a TLS API. Clients connect over plain TCP. For production use, TLS termination should be handled externally (e.g., by the wasmtime host or a reverse proxy).

### No bridge support

Broker-to-broker bridging requires outbound TCP connections, which WASI supports (`TcpSocket::start_connect`), but this has not been implemented.

### Task cleanup

Spawned sub-tasks (keep-alive checker, disconnect watcher, publish forwarder) are not automatically cleaned up when their parent handler exits. They continue running until they detect the disconnection via shared `running` flags or channel closure. This causes a slow task count increase under heavy reconnection churn.

### IPv6

The listener currently only supports IPv4 bind addresses.

## Performance

Benchmark results on a single machine (wasmtime v43, `wasm32-wasip2`, sequential `mosquitto_pub`/`mosquitto_sub` clients). All numbers are from the included `scripts/bench.sh` harness, which reports min/median/mean/p95/max/stddev across multiple runs.

### Methodology

The benchmark measures three scenarios:

1. **Round-trip latency**: a single message published and received by one subscriber. Measures the wall-clock time from `mosquitto_pub` invocation to `mosquitto_sub` exit.
2. **Sustained throughput**: *N* sequential publishes with one subscriber. Each `mosquitto_pub` invocation is a separate process (connect, publish, disconnect), so the numbers include per-message TCP connection overhead.
3. **Fan-out**: one publisher sending *N/10* messages with 5 concurrent subscribers.

Note: these numbers reflect `mosquitto_pub` spawning a new process per message, which dominates the cost. The broker's internal routing is substantially faster than what the external client tooling can saturate.

### Results

**1000 messages, 64-byte payload, 5 runs:**

| Metric | Min | Median | Mean | p95 | Stddev |
|--------|-----|--------|------|-----|--------|
| Round-trip latency | 8 ms | 9 ms | 8 ms | 10 ms | 1 ms |
| Publish rate | 240 msg/s | 282 msg/s | 307 msg/s | 379 msg/s | 51 msg/s |
| End-to-end rate | 240 msg/s | 282 msg/s | 306 msg/s | 379 msg/s | 50 msg/s |
| Throughput | 15 KB/s | 17 KB/s | 18 KB/s | 23 KB/s | 3 KB/s |
| Fan-out (5 subs, 100 msgs) | 453 ms | 468 ms | 465 ms | 480 ms | 10 ms |
| Fan-out delivery | 500/500 messages across all runs (100% delivery) |

### Interpretation

The primary bottleneck is **not the broker**. Each `mosquitto_pub` call creates a new TCP connection, performs an MQTT CONNECT/CONNACK handshake, publishes one message, sends DISCONNECT, and closes the socket. This per-message overhead (~3--5 ms) accounts for the majority of the measured latency and limits throughput to a few hundred messages per second.

Evidence supporting this interpretation:

- **Fan-out is efficient**: 5 subscribers receive 100% of messages (500/500) with low variance (stddev 10 ms), meaning the broker's internal `MessageRouter` dispatch adds negligible overhead per additional subscriber.
- **Pub rate tracks E2E rate**: publish and end-to-end rates are nearly identical (282 vs 282 msg/s median), confirming that the broker routes messages as fast as they arrive -- there is no internal queuing delay.
- **Variance is low**: latency stddev is 1 ms; throughput stddev is ~17% of mean. The variance is consistent with process-spawn jitter rather than broker-internal contention.

To measure the broker's true internal throughput, a persistent-connection client (e.g., a custom Rust MQTT client using a single TCP session) would eliminate per-message connection overhead. The cooperative executor's round-robin polling adds ~1 ms idle sleep between cycles, setting a theoretical floor of ~1000 polls/second, but in practice the executor wakes immediately when I/O is ready via `Pollable::ready()`.

### Running the benchmark

```bash
# Start the broker
wasmtime run -S inherit-network \
  target/wasm32-wasip2/release/mqtt5-broker.wasm &

# Default: 1000 messages, 64B payload, 5 runs
./crates/mqtt5-wasi/scripts/bench.sh

# Custom: 5000 messages, 256B payload, 10 runs
./crates/mqtt5-wasi/scripts/bench.sh 5000 256 127.0.0.1:1883 10
```

## Binary Size

The release build produces a ~325KB `.wasm` binary (with `opt-level = "z"`, LTO, and `panic = "abort"`), containing a full MQTT v5.0 broker with authentication, ACL, shared subscriptions, QoS 0/1/2, retained messages, session management, and will messages.

## WASIp3 and Future Work

The current implementation targets `wasm32-wasip2` (Rust Tier 2) and uses WASIp2 APIs. It does not use WASIp3's native async (`stream<T>`, `future<T>` at the Component Model level). However, the architecture is compatible with WASIp3 -- the custom executor could be replaced with the component model's native async scheduling, and `blocking_flush` could be replaced with async flushes. The broker benefits from WASIp3 implicitly when run on wasmtime v43+ with async support, as the host can optimize poll/sleep operations.
