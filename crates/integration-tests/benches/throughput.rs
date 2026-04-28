//! Throughput benchmarks for the expose-rs tunnel.
//!
//! # Environment (recorded at benchmark creation time)
//!
//! CPU model : AMD EPYC 7763 64-Core Processor
//! Logical CPUs (nproc) : 4
//! Physical cores per socket : 2
//! Siblings (HT threads) : 4
//!
//! # Measured throughput (optimized build, loopback, same machine)
//!
//! ## single_connection_throughput
//! | Payload size | Throughput (median) |
//! |--------------|---------------------|
//! | 16 KiB       | ~195 KiB/s          |
//! | 256 KiB      | ~5.2 MiB/s          |
//! | 1 MiB        | ~19 MiB/s           |
//!
//! ## concurrent_connections_throughput  (64 KiB per connection, aggregate)
//! | Connections | Aggregate throughput (median) |
//! |-------------|-------------------------------|
//! | 1           | ~1.4 MiB/s                    |
//! | 4           | ~5.5 MiB/s                    |
//! | 16          | ~21 MiB/s                     |
//!
//! Run with:
//!   cargo bench -p expose-integration-tests
//!
//! The benchmarks spin up a full server + client tunnel over loopback and
//! measure how many bytes per second flow through it.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

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

// ── Benchmarks ────────────────────────────────────────────────────────────────

/// Single-connection throughput: send `payload_size` bytes through the tunnel,
/// wait for the full echo, and report bytes/second.
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

    for &size in &[16 * 1024usize, 256 * 1024, 1024 * 1024] {
        let payload: Vec<u8> = (0u32..).map(|i| (i % 251) as u8).take(size).collect();
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_bytes", size)),
            &payload,
            |b, payload| {
                b.to_async(&rt).iter(|| async {
                    let mut stream = TcpStream::connect(server_addr).await.unwrap();
                    stream.write_all(payload).await.unwrap();

                    let mut received = vec![0u8; payload.len()];
                    stream.read_exact(&mut received).await.unwrap();
                    received
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

/// Concurrent-connection throughput: open `num_conns` connections in parallel,
/// each sending `payload_size` bytes, and measure aggregate bytes/second.
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

    const PAYLOAD_SIZE: usize = 64 * 1024;
    let payload: Vec<u8> = (0u32..).map(|i| (i % 251) as u8).take(PAYLOAD_SIZE).collect();

    for &num_conns in &[1usize, 4, 16] {
        let total_bytes = (PAYLOAD_SIZE * num_conns) as u64;
        group.throughput(Throughput::Bytes(total_bytes));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_conns", num_conns)),
            &(num_conns, payload.clone()),
            |b, (num_conns, payload)| {
                let num_conns = *num_conns;
                b.to_async(&rt).iter(|| async {
                    let mut handles = Vec::with_capacity(num_conns);
                    for _ in 0..num_conns {
                        let p = payload.clone();
                        handles.push(tokio::spawn(async move {
                            let mut stream = TcpStream::connect(server_addr).await.unwrap();
                            stream.write_all(&p).await.unwrap();
                            let mut received = vec![0u8; p.len()];
                            stream.read_exact(&mut received).await.unwrap();
                            received
                        }));
                    }
                    for h in handles {
                        h.await.unwrap();
                    }
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

criterion_group!(
    benches,
    bench_single_connection_throughput,
    bench_concurrent_connections_throughput,
);
criterion_main!(benches);
