# mqtt5-wasi

Standalone MQTT v5.0 / v3.1.1 **broker and client** that run as WebAssembly components inside [wasmtime](https://wasmtime.dev/) using WASI Preview 2 APIs. Both run on the [`wstd`](https://github.com/bytecodealliance/wstd) async runtime — its `TcpListener` / `TcpStream` provide non-blocking `wasi:sockets/tcp` I/O, and `wstd::runtime::block_on` / `spawn` schedule the broker's per-client tasks. The wire-level packet types come from `mqtt5-protocol`, and the broker reuses the transport-agnostic internals from the `mqtt5` crate (router, storage, auth, ACL).

## Features

- **`wstd`-powered runtime** — Single-threaded cooperative reactor over WASI Preview 2; consumers can mix their own async code on the same scheduler
- **WASI TCP sockets** — Non-blocking I/O via `wasi:sockets/tcp` (not `std::net`)
- **Full MQTT v5.0 broker** — QoS 0/1/2, retained messages, shared subscriptions, will messages, session management
- **Minimal MQTT v5.0 / v3.1.1 client** — connect, publish, subscribe, recv, disconnect
- **Authentication & ACL** — Anonymous, password, and comprehensive auth with access control (broker)
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
use mqtt5_wasi::{wstd, WasiClient, WasiClientConfig};
use mqtt5_protocol::QoS;

wstd::runtime::block_on(async {
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

    client.disconnect().await.expect("disconnect");
});
```

The client accepts both literal `ip:port` strings and `host:port` (resolved through `wasi:sockets/ip-name-lookup` via `std::net::ToSocketAddrs`).

`recv()` drives the inbound side of the QoS handshake (sending `PUBACK`/`PUBREC`/`PUBCOMP` as needed) and absorbs `PINGRESP`, returning only user-visible `PUBLISH` packets. A keep-alive task spawned at connect time sends `PINGREQ` automatically; setting `keep_alive` to `Duration::ZERO` disables it.

## Sharing the runtime with your own code

`wstd` is re-exported as `mqtt5_wasi::wstd`, so the same reactor that powers the broker and client can drive your own async work:

```rust
use mqtt5_wasi::wstd;

wstd::runtime::block_on(async {
    wstd::runtime::spawn(async {
        wstd::task::sleep(wstd::time::Duration::from_secs(1)).await;
        println!("background task");
    })
    .detach();

    // ...run a WasiBroker::serve / WasiClient here on the same reactor...
});
```

If you prefer to embed the broker inside an existing `block_on`, call [`WasiBroker::serve`](src/broker.rs) (async) instead of [`WasiBroker::run`](src/broker.rs) (which starts its own `block_on`).

## Limitations

- **No TLS** — WASI Preview 2 has no TLS API; use external TLS termination
- **No persistent storage** — Uses `MemoryBackend` only; sessions are lost on restart (broker)
- **IPv4 only** — `wstd::net::TcpStream::connect_addr` does not yet support IPv6, and the broker currently only binds IPv4
- **No bridge support** — Broker-to-broker bridging is not implemented
- **Client is intentionally minimal** — no will message, no enhanced auth flow, no automatic reconnect

## Documentation

See the [main repository](https://github.com/LabOverWire/mqtt-lib) for complete documentation and examples.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
