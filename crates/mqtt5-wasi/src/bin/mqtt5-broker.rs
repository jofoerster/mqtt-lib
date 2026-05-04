#[cfg(target_os = "wasi")]
fn main() {
    let bind_addr = std::env::var("MQTT_BIND").unwrap_or_else(|_| "0.0.0.0:1883".to_string());
    mqtt5_wasi::WasiBroker::default_anonymous().run(&bind_addr);
}

#[cfg(not(target_os = "wasi"))]
fn main() {
    eprintln!("mqtt5-broker is a WASI binary; build it with --target wasm32-wasip2");
    std::process::exit(1);
}
