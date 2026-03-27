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

## Binary Size

The release build produces a ~325KB `.wasm` binary (with `opt-level = "z"`, LTO, and `panic = "abort"`), containing a full MQTT v5.0 broker with authentication, ACL, shared subscriptions, QoS 0/1/2, retained messages, session management, and will messages.

## WASIp3 and Future Work

The current implementation targets `wasm32-wasip2` (Rust Tier 2) and uses WASIp2 APIs. It does not use WASIp3's native async (`stream<T>`, `future<T>` at the Component Model level). However, the architecture is compatible with WASIp3 -- the custom executor could be replaced with the component model's native async scheduling, and `blocking_flush` could be replaced with async flushes. The broker benefits from WASIp3 implicitly when run on wasmtime v43+ with async support, as the host can optimize poll/sleep operations.
