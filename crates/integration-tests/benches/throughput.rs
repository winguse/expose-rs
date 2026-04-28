//! Throughput benchmarks for the expose-rs tunnel.
//!
//! # Environment (recorded at benchmark creation time)
//!
//! CPU model : AMD EPYC 7763 64-Core Processor
//! Logical CPUs (nproc) : 4
//! Physical cores per socket : 2
//! Siblings (HT threads) : 4
//!
//! # Benchmark design
//!
//! Each benchmark keeps a **single persistent TCP connection** open across all
//! iterations so that per-connection setup cost (WebSocket handshake, etc.) is
//! not included in the throughput measurement.  Criterion is configured with a
//! 5-second measurement window; it drives as many send-echo round-trips as
//! possible and reports the average throughput (bytes/second) across that
//! window.
//!
//! Payload sizes tested: 512 B, 1 KiB, 16 KiB, 64 KiB, 1 MiB.
//!
//! ## single_connection_throughput — via tunnel vs direct baseline
//!
//! | Payload size | Via tunnel (avg/s) | Direct baseline (avg/s) |
//! |--------------|--------------------|-------------------------|
//! | 512 B        | TBD                | TBD                     |
//! | 1 KiB        | TBD                | TBD                     |
//! | 16 KiB       | TBD                | TBD                     |
//! | 64 KiB       | TBD                | TBD                     |
//! | 1 MiB        | TBD                | TBD                     |
//!
//! ## concurrent_connections_throughput — via tunnel vs direct baseline
//! (64 KiB per connection, aggregate across all connections)
//!
//! | Connections | Via tunnel (avg/s) | Direct baseline (avg/s) |
//! |-------------|--------------------|-------------------------|
//! | 1           | TBD                | TBD                     |
//! | 4           | TBD                | TBD                     |
//! | 16          | TBD                | TBD                     |
//!
//! The "direct" benchmarks connect straight to the TCP echo server with no
//! expose-rs server or client in the path, giving a loopback baseline.
//!
//! Run with:
//!   cargo bench -p expose-integration-tests
//!
//! The tunnel benchmarks spin up a full server + client over loopback and
//! measure how many bytes per second flow through it.

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

// ── Payload helpers ───────────────────────────────────────────────────────────

/// Payload sizes to benchmark.
const PAYLOAD_SIZES: &[usize] = &[512, 1024, 16 * 1024, 64 * 1024, 1024 * 1024];

/// Prime modulus used when generating payload bytes.
///
/// A prime that does not divide any common buffer size (512, 1024, 4096 …)
/// ensures the repeating pattern never aligns with read/write boundaries,
/// giving a more realistic byte stream.
const PAYLOAD_MODULUS: u32 = 251;

/// Returns a human-readable label for a byte count (e.g. "512B", "1KiB", "64KiB").
fn size_label(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{}KiB", bytes / 1024)
    } else {
        format!("{}MiB", bytes / (1024 * 1024))
    }
}

/// Generates a deterministic payload of `size` bytes.
fn make_payload(size: usize) -> Vec<u8> {
    (0u32..).map(|i| (i % PAYLOAD_MODULUS) as u8).take(size).collect()
}

// ── Tunnel helpers ────────────────────────────────────────────────────────────

async fn start_echo_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        let (mut rx, mut tx) = stream.into_split();
                        tokio::io::copy(&mut rx, &mut tx).await.ok();
                    });
                }
                Err(_) => break,
            }
        }
    });
    addr
}

/// Spin up server + client and return the server's listening address.
/// The returned handles must be kept alive for the duration of the benchmark.
async fn start_tunnel(
    upstream: std::net::SocketAddr,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let secret = "bench-secret".to_string();

    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();

    let server_task = tokio::spawn(expose_server::run_server_with_channel_config(
        server_listener,
        secret.clone(),
        expose_server::CapacityConfig::default(),
    ));

    tokio::time::sleep(Duration::from_millis(20)).await;

    let ws_url = format!("ws://{}/{}", server_addr, secret);
    let client_task = tokio::spawn(expose_client::run_client_once_with_channel_config(
        ws_url,
        upstream.to_string(),
        expose_client::CapacityConfig::default(),
    ));

    // Wait until the tunnel is ready.
    let probe = b"probe";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("Tunnel did not become ready within 5 s");
        }
        if let Ok(Ok(mut stream)) =
            tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(server_addr)).await
        {
            let _ = stream.write_all(probe).await;
            let mut buf = [0u8; 5];
            if tokio::time::timeout(Duration::from_millis(200), stream.read_exact(&mut buf))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false)
                && &buf == probe
            {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    (server_addr, server_task, client_task)
}

// ── Shared benchmark configuration ────────────────────────────────────────────

/// Measurement window for every benchmark group.
const MEASUREMENT_SECS: u64 = 5;
/// Warm-up window before each measurement.
const WARMUP_SECS: u64 = 1;

// ── Benchmarks ────────────────────────────────────────────────────────────────

/// Single-connection throughput via the tunnel.
///
/// A persistent TCP connection is kept open across all Criterion iterations so
/// that the WebSocket handshake cost is excluded from the measurement.  Criterion
/// is told to measure for `MEASUREMENT_SECS` seconds; it runs as many
/// send-echo round-trips as possible and derives average bytes/second.
fn bench_single_connection_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let (upstream, server_addr, server_task, client_task) = rt.block_on(async {
        let upstream = start_echo_server().await;
        let (server_addr, st, ct) = start_tunnel(upstream).await;
        (upstream, server_addr, st, ct)
    });

    let mut group = c.benchmark_group("single_connection_throughput");
    group.measurement_time(Duration::from_secs(MEASUREMENT_SECS));
    group.warm_up_time(Duration::from_secs(WARMUP_SECS));

    for &size in PAYLOAD_SIZES {
        let payload = make_payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(size_label(size)),
            &payload,
            |b, payload| {
                let payload = payload.clone();
                b.iter_custom(|iters| {
                    let payload = payload.clone();
                    rt.block_on(async move {
                        let mut stream = TcpStream::connect(server_addr).await.unwrap();
                        let mut buf = vec![0u8; payload.len()];
                        let start = Instant::now();
                        for _ in 0..iters {
                            stream.write_all(&payload).await.unwrap();
                            stream.read_exact(&mut buf).await.unwrap();
                        }
                        start.elapsed()
                    })
                });
            },
        );
    }

    group.finish();

    rt.block_on(async {
        server_task.abort();
        client_task.abort();
        let _ = upstream;
    });
}

/// Concurrent-connection throughput via the tunnel.
///
/// Opens `num_conns` persistent connections in parallel, each repeatedly
/// sending a 64 KiB chunk and reading the echo for `MEASUREMENT_SECS` seconds.
/// The reported throughput is the aggregate across all connections.
fn bench_concurrent_connections_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let (upstream, server_addr, server_task, client_task) = rt.block_on(async {
        let upstream = start_echo_server().await;
        let (server_addr, st, ct) = start_tunnel(upstream).await;
        (upstream, server_addr, st, ct)
    });

    let mut group = c.benchmark_group("concurrent_connections_throughput");
    group.measurement_time(Duration::from_secs(MEASUREMENT_SECS));
    group.warm_up_time(Duration::from_secs(WARMUP_SECS));

    const PAYLOAD_SIZE: usize = 64 * 1024;
    let payload = make_payload(PAYLOAD_SIZE);

    for &num_conns in &[1usize, 4, 16] {
        // Criterion measures time per logical "iteration"; each iteration sends
        // one chunk on every connection, so throughput = num_conns * chunk_size.
        group.throughput(Throughput::Bytes((PAYLOAD_SIZE * num_conns) as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_conns", num_conns)),
            &(num_conns, payload.clone()),
            |b, (num_conns, payload)| {
                let num_conns = *num_conns;
                let payload = payload.clone();
                b.iter_custom(|iters| {
                    let payload = payload.clone();
                    rt.block_on(async move {
                        // Open all connections before starting the clock.
                        let mut streams = Vec::with_capacity(num_conns);
                        for _ in 0..num_conns {
                            streams.push(TcpStream::connect(server_addr).await.unwrap());
                        }

                        let start = Instant::now();
                        for _ in 0..iters {
                            let mut handles = Vec::with_capacity(num_conns);
                            for stream in streams.drain(..) {
                                let p = payload.clone();
                                handles.push(tokio::spawn(async move {
                                    let mut s = stream;
                                    let mut buf = vec![0u8; p.len()];
                                    s.write_all(&p).await.unwrap();
                                    s.read_exact(&mut buf).await.unwrap();
                                    s
                                }));
                            }
                            for h in handles {
                                streams.push(h.await.unwrap());
                            }
                        }
                        start.elapsed()
                    })
                });
            },
        );
    }

    group.finish();

    rt.block_on(async {
        server_task.abort();
        client_task.abort();
        let _ = upstream;
    });
}

// ── Baseline benchmarks (direct connection, no tunnel) ────────────────────────

/// Single-connection throughput over a direct TCP connection to the echo server,
/// with no expose-rs tunnel in the path.  Used as a baseline to quantify the
/// overhead introduced by the WebSocket tunnel.
fn bench_single_connection_throughput_direct(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let echo_addr = rt.block_on(start_echo_server());

    let mut group = c.benchmark_group("single_connection_throughput_direct");
    group.measurement_time(Duration::from_secs(MEASUREMENT_SECS));
    group.warm_up_time(Duration::from_secs(WARMUP_SECS));

    for &size in PAYLOAD_SIZES {
        let payload = make_payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(size_label(size)),
            &payload,
            |b, payload| {
                let payload = payload.clone();
                b.iter_custom(|iters| {
                    let payload = payload.clone();
                    rt.block_on(async move {
                        let mut stream = TcpStream::connect(echo_addr).await.unwrap();
                        let mut buf = vec![0u8; payload.len()];
                        let start = Instant::now();
                        for _ in 0..iters {
                            stream.write_all(&payload).await.unwrap();
                            stream.read_exact(&mut buf).await.unwrap();
                        }
                        start.elapsed()
                    })
                });
            },
        );
    }

    group.finish();
}

/// Concurrent-connection throughput over direct TCP connections, no tunnel.
fn bench_concurrent_connections_throughput_direct(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let echo_addr = rt.block_on(start_echo_server());

    let mut group = c.benchmark_group("concurrent_connections_throughput_direct");
    group.measurement_time(Duration::from_secs(MEASUREMENT_SECS));
    group.warm_up_time(Duration::from_secs(WARMUP_SECS));

    const PAYLOAD_SIZE: usize = 64 * 1024;
    let payload = make_payload(PAYLOAD_SIZE);

    for &num_conns in &[1usize, 4, 16] {
        group.throughput(Throughput::Bytes((PAYLOAD_SIZE * num_conns) as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_conns", num_conns)),
            &(num_conns, payload.clone()),
            |b, (num_conns, payload)| {
                let num_conns = *num_conns;
                let payload = payload.clone();
                b.iter_custom(|iters| {
                    let payload = payload.clone();
                    rt.block_on(async move {
                        let mut streams = Vec::with_capacity(num_conns);
                        for _ in 0..num_conns {
                            streams.push(TcpStream::connect(echo_addr).await.unwrap());
                        }

                        let start = Instant::now();
                        for _ in 0..iters {
                            let mut handles = Vec::with_capacity(num_conns);
                            for stream in streams.drain(..) {
                                let p = payload.clone();
                                handles.push(tokio::spawn(async move {
                                    let mut s = stream;
                                    let mut buf = vec![0u8; p.len()];
                                    s.write_all(&p).await.unwrap();
                                    s.read_exact(&mut buf).await.unwrap();
                                    s
                                }));
                            }
                            for h in handles {
                                streams.push(h.await.unwrap());
                            }
                        }
                        start.elapsed()
                    })
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_single_connection_throughput,
    bench_concurrent_connections_throughput,
    bench_single_connection_throughput_direct,
    bench_concurrent_connections_throughput_direct,
);
criterion_main!(benches);
