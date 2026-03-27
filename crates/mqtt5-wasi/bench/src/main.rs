use mqtt5::client::MqttClient;
use mqtt5::types::ConnectOptions;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_BROKER: &str = "mqtt://127.0.0.1:1883";

struct Config {
    broker: String,
    messages: u64,
    payload_size: usize,
    runs: u64,
    subscribers: u64,
    warmup: u64,
}

impl Config {
    fn from_args() -> Self {
        let mut args = std::env::args().skip(1);
        Self {
            messages: args
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10_000),
            payload_size: args.next().and_then(|s| s.parse().ok()).unwrap_or(64),
            runs: args.next().and_then(|s| s.parse().ok()).unwrap_or(5),
            subscribers: args.next().and_then(|s| s.parse().ok()).unwrap_or(1),
            broker: args
                .next()
                .unwrap_or_else(|| DEFAULT_BROKER.to_string()),
            warmup: 100,
        }
    }
}

struct RunResult {
    pub_duration: Duration,
    e2e_duration: Duration,
    published: u64,
    received: u64,
}

struct Stats {
    min: f64,
    max: f64,
    median: f64,
    mean: f64,
    p95: f64,
    stddev: f64,
}

fn compute_stats(values: &[f64]) -> Stats {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    let sum: f64 = sorted.iter().sum();
    let mean = sum / n as f64;
    let variance = sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
    Stats {
        min: sorted[0],
        max: sorted[n - 1],
        median: sorted[n / 2],
        mean,
        p95: sorted[(n * 95 / 100).min(n - 1)],
        stddev: variance.sqrt(),
    }
}

fn print_stats(label: &str, stats: &Stats, unit: &str) {
    println!(
        "  {label:<14} min={:<10} median={:<10} mean={:<10} p95={:<10} max={:<10} stddev={:<10}",
        format!("{:.1}{unit}", stats.min),
        format!("{:.1}{unit}", stats.median),
        format!("{:.1}{unit}", stats.mean),
        format!("{:.1}{unit}", stats.p95),
        format!("{:.1}{unit}", stats.max),
        format!("{:.1}{unit}", stats.stddev),
    );
}

async fn connect_client(broker: &str, client_id: &str) -> MqttClient {
    let opts = ConnectOptions::new(client_id);
    let client = MqttClient::with_options(opts);
    client.connect(broker).await.expect("Failed to connect");
    client
}

async fn measure_latency(broker: &str, run: u64) -> Duration {
    let received = Arc::new(AtomicU64::new(0));
    let received_clone = Arc::clone(&received);

    let sub = connect_client(broker, &format!("bench-lat-sub-{run}")).await;
    sub.subscribe("bench/latency", move |_| {
        received_clone.fetch_add(1, Ordering::Relaxed);
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let pub_client = connect_client(broker, &format!("bench-lat-pub-{run}")).await;

    let start = Instant::now();
    pub_client
        .publish("bench/latency", b"ping".to_vec())
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while received.load(Ordering::Relaxed) == 0 {
        if Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_micros(100)).await;
    }
    let elapsed = start.elapsed();

    pub_client.disconnect().await.ok();
    sub.disconnect().await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    elapsed
}

async fn measure_throughput(
    broker: &str,
    msg_count: u64,
    payload_size: usize,
    num_subscribers: u64,
    warmup: u64,
    run: u64,
) -> RunResult {
    let payload: Vec<u8> = vec![0xAB; payload_size];
    let received = Arc::new(AtomicU64::new(0));

    // Connect subscribers
    let mut subs = Vec::new();
    for i in 0..num_subscribers {
        let sub = connect_client(broker, &format!("bench-sub-{run}-{i}")).await;
        let counter = Arc::clone(&received);
        sub.subscribe("bench/throughput", move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        })
        .await
        .unwrap();
        subs.push(sub);
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect publisher
    let pub_client = connect_client(broker, &format!("bench-pub-{run}")).await;

    // Warmup
    for _ in 0..warmup {
        if pub_client
            .publish("bench/throughput", payload.clone())
            .await
            .is_err()
        {
            break;
        }
    }
    let warmup_expected = warmup * num_subscribers;
    let warmup_deadline = Instant::now() + Duration::from_secs(10);
    while received.load(Ordering::Relaxed) < warmup_expected {
        if Instant::now() > warmup_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    received.store(0, Ordering::Relaxed);

    // Measured run
    let start = Instant::now();
    let mut published = 0u64;
    for _ in 0..msg_count {
        match pub_client
            .publish("bench/throughput", payload.clone())
            .await
        {
            Ok(_) => published += 1,
            Err(_) => break,
        }
    }
    let pub_duration = start.elapsed();

    // Wait for delivery (with timeout)
    let expected = published * num_subscribers;
    let deadline = Instant::now() + Duration::from_secs(30);
    while received.load(Ordering::Relaxed) < expected {
        if Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let e2e_duration = start.elapsed();
    let final_received = received.load(Ordering::Relaxed);

    // Disconnect (ignore errors -- broker may have already closed)
    pub_client.disconnect().await.ok();
    for sub in &subs {
        sub.disconnect().await.ok();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    RunResult {
        pub_duration,
        e2e_duration,
        published,
        received: final_received,
    }
}

#[tokio::main]
async fn main() {
    let cfg = Config::from_args();

    println!("=== WASI MQTT Broker Benchmark (persistent connections) ===");
    println!("Broker:       {}", cfg.broker);
    println!("Messages:     {}", cfg.messages);
    println!("Payload:      {} bytes", cfg.payload_size);
    println!("Subscribers:  {}", cfg.subscribers);
    println!("Runs:         {}", cfg.runs);
    println!("Warmup:       {} messages", cfg.warmup);
    println!();

    // --- Latency ---
    println!(
        "--- Round-trip latency (single message, {} runs) ---",
        cfg.runs
    );
    let mut latencies = Vec::new();
    for r in 0..cfg.runs {
        let lat = measure_latency(&cfg.broker, r).await;
        latencies.push(lat.as_secs_f64() * 1000.0);
    }
    print_stats("Latency:", &compute_stats(&latencies), "ms");
    println!();

    // --- Throughput ---
    println!(
        "--- Throughput ({} messages, {} subs, {} runs) ---",
        cfg.messages, cfg.subscribers, cfg.runs
    );
    let mut pub_rates = Vec::new();
    let mut e2e_rates = Vec::new();
    let mut throughputs = Vec::new();
    let mut delivery_pcts = Vec::new();

    for r in 0..cfg.runs {
        let result = measure_throughput(
            &cfg.broker,
            cfg.messages,
            cfg.payload_size,
            cfg.subscribers,
            cfg.warmup,
            r,
        )
        .await;

        let pub_secs = result.pub_duration.as_secs_f64();
        let e2e_secs = result.e2e_duration.as_secs_f64();

        if pub_secs > 0.0 {
            pub_rates.push(result.published as f64 / pub_secs);
        }
        if e2e_secs > 0.0 {
            e2e_rates.push(result.received as f64 / e2e_secs);
            throughputs.push(
                result.received as f64 * cfg.payload_size as f64 / e2e_secs / 1024.0,
            );
        }

        let expected = result.published * cfg.subscribers;
        if expected > 0 {
            delivery_pcts.push(result.received as f64 / expected as f64 * 100.0);
        }

        println!(
            "  run {}: published={} received={}/{} pub={:.0}msg/s e2e={:.0}msg/s",
            r + 1,
            result.published,
            result.received,
            expected,
            result.published as f64 / pub_secs,
            result.received as f64 / e2e_secs,
        );
    }

    println!();
    if !pub_rates.is_empty() {
        print_stats("Pub rate:", &compute_stats(&pub_rates), " msg/s");
    }
    if !e2e_rates.is_empty() {
        print_stats("E2E rate:", &compute_stats(&e2e_rates), " msg/s");
    }
    if !throughputs.is_empty() {
        print_stats("Throughput:", &compute_stats(&throughputs), " KB/s");
    }
    if !delivery_pcts.is_empty() {
        print_stats("Delivery:", &compute_stats(&delivery_pcts), "%");
    }
    println!();

    // --- Fan-out ---
    if cfg.subscribers == 1 {
        let fan_subs: u64 = 5;
        let fan_msgs = cfg.messages / 10;
        println!(
            "--- Fan-out (1 pub -> {fan_subs} subs, {fan_msgs} messages, {} runs) ---",
            cfg.runs
        );
        let mut fan_rates = Vec::new();
        let mut fan_deliveries = Vec::new();

        for r in 0..cfg.runs {
            let result = measure_throughput(
                &cfg.broker,
                fan_msgs,
                cfg.payload_size,
                fan_subs,
                cfg.warmup,
                100 + r,
            )
            .await;

            let e2e_secs = result.e2e_duration.as_secs_f64();
            if e2e_secs > 0.0 {
                fan_rates.push(result.received as f64 / e2e_secs);
            }
            let expected = result.published * fan_subs;
            if expected > 0 {
                fan_deliveries.push(result.received as f64 / expected as f64 * 100.0);
            }
        }

        if !fan_rates.is_empty() {
            print_stats("Fan rate:", &compute_stats(&fan_rates), " msg/s");
        }
        if !fan_deliveries.is_empty() {
            print_stats("Delivery:", &compute_stats(&fan_deliveries), "%");
        }
        println!();
    }

    println!("=== Done ===");
}
