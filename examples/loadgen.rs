//! Minimal HTTP load generator for the ingest edge (claim-3 harness).
//!
//! Persistent keep-alive TCP connections, so the number reflects Hoglet's
//! throughput, not a per-request process spawn. Not a published benchmark —
//! a local number to track against SPEC.md targets and catch regressions.
//!
//! Usage: cargo run --release --example loadgen -- <port> <total> <threads>

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8000);
    let total: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let threads: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);

    let per = total / threads;
    let ok = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let ok = ok.clone();
            std::thread::spawn(move || {
                let mut stream =
                    TcpStream::connect(("127.0.0.1", port)).expect("connect");
                stream.set_nodelay(true).ok();
                let mut buf = [0u8; 4096];
                for i in 0..per {
                    let body = format!(
                        r#"{{"api_key":"phc_load","batch":[{{"event":"evt_{}","distinct_id":"u_{}"}}]}}"#,
                        i % 50,
                        (t * per + i) % 5000
                    );
                    let req = format!(
                        "POST /batch/ HTTP/1.1\r\nHost: h\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if stream.write_all(req.as_bytes()).is_err() {
                        break;
                    }
                    // Read one response (status line + headers + body). The
                    // server closes nothing on keep-alive; a single read of
                    // the small 200 body is enough to stay in lockstep.
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            if buf[..n].starts_with(b"HTTP/1.1 200") {
                                ok.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        _ => break,
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().ok();
    }

    let elapsed = start.elapsed().as_secs_f64();
    let acked = ok.load(Ordering::Relaxed);
    let rate = (acked as f64 / elapsed) as u64;
    println!("acked {acked}/{total} in {elapsed:.2}s = {rate} events/s");
}
