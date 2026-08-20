//! Process-boundary durability: every event acknowledged with a 2xx remains
//! queryable after SIGKILL and a normal production restart.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read the ephemeral port")
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
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("hoglet did not come up on port {port}");
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    body: &Value,
    extra_headers: &[(&str, &str)],
) -> Option<String> {
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut headers = String::new();
    for (name, value) in extra_headers {
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(value);
        headers.push_str("\r\n");
    }
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

fn status(response: &str) -> Option<u16> {
    response
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn response_body(response: &str) -> Option<Value> {
    serde_json::from_str(response.split_once("\r\n\r\n")?.1).ok()
}

fn response_cookie(response: &str) -> Option<String> {
    response.lines().find_map(|line| {
        let value = line
            .strip_prefix("set-cookie: ")
            .or_else(|| line.strip_prefix("Set-Cookie: "))?;
        value.split(';').next().map(str::to_owned)
    })
}

fn post_event(port: u16, i: usize) -> bool {
    request(
        port,
        "POST",
        "/e/",
        &json!({
            "event": "crash_test",
            "distinct_id": format!("u{i}"),
            "token": "phc_crash",
            "timestamp": "2026-08-20T12:00:00Z"
        }),
        &[],
    )
    .and_then(|response| status(&response))
        == Some(200)
}

#[test]
fn sigkill_loses_no_acked_events() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let port = free_port();
    let mut first = spawn_hoglet(directory.path(), port);
    wait_until_up(port);

    let setup = request(
        port,
        "POST",
        "/api/auth/setup",
        &json!({
            "email": "owner@example.com",
            "password": "correct horse battery staple",
            "organization_name": "Crash Recovery",
            "project_name": "Durability",
            "existing_project_token": "phc_crash"
        }),
        &[],
    )
    .expect("setup response");
    assert_eq!(status(&setup), Some(200), "setup failed: {setup}");
    let cookie = response_cookie(&setup).expect("setup session cookie");
    let workspace = response_body(&setup).expect("workspace JSON");
    let project_id = workspace["organizations"][0]["projects"][0]["id"]
        .as_str()
        .expect("project id")
        .to_owned();

    let mut acknowledged = 0_u64;
    for i in 0..40 {
        if post_event(port, i) {
            acknowledged += 1;
        }
    }
    assert!(acknowledged > 0, "no events were acknowledged");

    first.kill().expect("SIGKILL first server");
    first.wait().expect("reap first server");

    // Restart through the real production composition. Its pipeline recovers
    // any acknowledged-but-unpublished WAL prefix before serving queries.
    let mut second = spawn_hoglet(directory.path(), port);
    wait_until_up(port);
    let query = json!({
        "query": {
            "kind": "Trends",
            "series": [{
                "event": {"type": "name", "value": "crash_test"},
                "math": {"type": "total"}
            }],
            "filters": {"op": "AND", "values": []},
            "range": {
                "from": "2026-08-20T00:00:00Z",
                "to": "2026-08-21T00:00:00Z"
            },
            "interval": "Day"
        },
        "refresh": true
    });
    let path = format!("/api/projects/{project_id}/query");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed = 0_u64;
    while Instant::now() < deadline {
        if let Some(response) = request(port, "POST", &path, &query, &[("Cookie", &cookie)])
            && status(&response) == Some(200)
            && let Some(body) = response_body(&response)
        {
            observed = body["results"][0]["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|point| point["count"].as_u64())
                .sum();
            if observed >= acknowledged {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    second.kill().expect("stop recovery server");
    second.wait().expect("reap recovery server");
    assert_eq!(
        observed, acknowledged,
        "an acknowledged event was lost across SIGKILL and restart"
    );
}
