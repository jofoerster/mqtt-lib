# mqtt5-wasi

Standalone MQTT v5.0 / v3.1.1 **broker and client** that run as WebAssembly components inside [wasmtime](https://wasmtime.dev/) using WASI Preview 2 APIs. Both share the same single-threaded cooperative executor and `wasi:sockets/tcp` transport, and reuse the wire-level packet types from `mqtt5-protocol`. The broker additionally reuses the transport-agnostic broker internals from the `mqtt5` crate (router, storage, auth, ACL).

## Features

- **WASI TCP sockets** — Non-blocking I/O via `wasi:sockets/tcp` (not `std::net`)
- **Full MQTT v5.0 broker** — QoS 0/1/2, retained messages, shared subscriptions, will messages, session management
- **Minimal MQTT v5.0 / v3.1.1 client** — connect, publish, subscribe, recv, disconnect — backed by the same executor and `WasiStream`
- **Authentication & ACL** — Anonymous, password, and comprehensive auth with access control (broker)
- **Cooperative concurrency** — Custom single-threaded async executor with `spawn()` and `block_on()`
- **Tiny binary** — ~325 KB `.wasm` release build for the broker
- **Change-only delivery** — Deduplicate repeated publishes on configured topic patterns (broker)
- **Echo suppression** — Prevent clients from receiving their own publishes (broker)
- **Load balancer mode** — Redirect clients via CONNACK `UseAnotherServer` (broker)

## Quick Start (broker)

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

## Broker usage

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

## Client usage

```rust
use mqtt5_wasi::{executor, WasiClient, WasiClientConfig};
use mqtt5_protocol::QoS;

executor::block_on(async {
    let config = WasiClientConfig::new("my-wasi-client")
        .with_keep_alive(std::time::Duration::from_secs(30));

    let mut client = WasiClient::connect("127.0.0.1:1883", &config).await
        .expect("connect");

    client.subscribe("test/#", QoS::AtLeastOnce).await.expect("sub");
    client.publish("test/hello", b"hello from wasi".to_vec(), QoS::AtLeastOnce)
        .await
        .expect("pub");

    while let Ok(Some(msg)) = client.recv().await {
        println!("{} = {}", msg.topic_name, String::from_utf8_lossy(&msg.payload));
    }

    client.disconnect().expect("disconnect");
});
```

The client accepts both literal `ip:port` strings and `host:port` (resolved through `wasi:sockets/ip-name-lookup`). For IPv6 literals use the bracketed form: `[::1]:1883`.

`recv()` drives the inbound side of the QoS handshake (sending `PUBACK`/`PUBREC`/`PUBCOMP` as needed) and absorbs `PINGRESP`, returning only user-visible `PUBLISH` packets. A keep-alive task spawned at connect time sends `PINGREQ` automatically; setting `keep_alive` to `Duration::ZERO` disables it.

## Modules exposed for advanced use

The cooperative async runtime is available as `mqtt5_wasi::executor` (`block_on`, `spawn`, `yield_now`) and `mqtt5_wasi::timer::sleep`, so you can drive your own `async` code on the same scheduler that powers the broker and client.

## Limitations

- **No TLS** — WASI Preview 2 has no TLS API; use external TLS termination
- **No persistent storage** — Uses `MemoryBackend` only; sessions are lost on restart (broker)
- **IPv4 listen only** — IPv6 bind addresses are not yet supported by the broker (the client supports both)
- **No bridge support** — Broker-to-broker bridging is not implemented
- **Client is intentionally minimal** — no will message, no enhanced auth flow, no automatic reconnect

## Documentation

See the [main repository](https://github.com/LabOverWire/mqtt-lib) for complete documentation and examples.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
