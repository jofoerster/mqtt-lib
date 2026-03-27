# mqtt5-wasi

Standalone MQTT v5.0 / v3.1.1 broker that runs as a WebAssembly component inside [wasmtime](https://wasmtime.dev/) using WASI Preview 2 APIs. Reuses the transport-agnostic broker internals from the `mqtt5` crate (router, storage, auth, ACL) with a custom single-threaded cooperative executor for the WASI environment.

## Features

- **WASI TCP sockets** - Non-blocking I/O via `wasi:sockets/tcp` (not `std::net`)
- **Full MQTT v5.0 broker** - QoS 0/1/2, retained messages, shared subscriptions, will messages, session management
- **Authentication & ACL** - Anonymous, password, and comprehensive auth with access control
- **Cooperative concurrency** - Custom single-threaded async executor with `spawn()` and `block_on()`
- **Tiny binary** - ~325 KB `.wasm` release build
- **Change-only delivery** - Deduplicate repeated publishes on configured topic patterns
- **Echo suppression** - Prevent clients from receiving their own publishes
- **Load balancer mode** - Redirect clients via CONNACK `UseAnotherServer`

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

## Usage

```rust
use mqtt5_wasi::{WasiBroker, WasiBrokerConfig};

let config = WasiBrokerConfig {
    allow_anonymous: true,
    max_clients: 100,
    ..WasiBrokerConfig::default()
};

let broker = WasiBroker::new(&config);
broker.run("0.0.0.0:1883");
```

## Limitations

- **No TLS** - WASI Preview 2 has no TLS API; use external TLS termination
- **No persistent storage** - Uses `MemoryBackend` only; sessions are lost on restart
- **IPv4 only** - IPv6 bind addresses are not yet supported
- **No bridge support** - Broker-to-broker bridging is not implemented

## Documentation

See the [main repository](https://github.com/LabOverWire/mqtt-lib) for complete documentation and examples.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
