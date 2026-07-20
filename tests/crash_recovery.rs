//! Claim 2 at the process boundary: every event acknowledged with a 2xx is
//! recoverable after a hard crash (SIGKILL, no shutdown hooks).
//!
//! A mock that acks-then-fsyncs passes every in-process test; only killing
//! the real binary catches it (claims.md).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_hoglet(data_dir: &std::path::Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_hoglet"))
        .env("HOGLET_DATA", data_dir)
        .env("HOGLET_ADDR", format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hoglet binary")
}

fn wait_until_up(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("hoglet did not come up on port {port}");
}

/// Minimal HTTP client: POST one event batch, return true on 200.
fn post_event(port: u16, i: usize) -> bool {
    let body =
        format!(r#"[{{"event":"crash_test_{i}","distinct_id":"u{i}","token":"phc_crash"}}]"#);
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let request = format!(
        "POST /e/ HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return false;
    }
    response.starts_with("HTTP/1.1 200")
}

#[test]
fn sigkill_loses_no_acked_events() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = spawn_hoglet(dir.path(), port);
    wait_until_up(port);

    // Stream events; count only the ones the server acknowledged.
    let mut acked = Vec::new();
    for i in 0..200 {
        if post_event(port, i) {
            acked.push(i);
        }
    }
    assert!(!acked.is_empty(), "no events were acknowledged");

    // Hard kill — no graceful shutdown, no flush hooks.
    child.kill().unwrap();
    child.wait().unwrap();

    // Recover the WAL directly and reconcile: every acked event must be
    // present. Unacked events may be lost; that's the contract.
    let (wal, runtime, recovered) =
        hoglet::wal::Wal::open(dir.path().join("wal")).expect("recover WAL after crash");
    drop(wal);
    runtime.close();

    let recovered_names: std::collections::HashSet<String> =
        recovered.events.iter().map(|e| e.event.clone()).collect();
    for i in &acked {
        assert!(
            recovered_names.contains(&format!("crash_test_{i}")),
            "acked event {i} lost after SIGKILL — durability contract broken"
        );
    }
}
