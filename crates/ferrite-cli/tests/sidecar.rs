use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn exchange(stream: &mut UnixStream, request: Value) -> Value {
    writeln!(stream, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

fn hello(stream: &mut UnixStream, id: u64) -> Value {
    exchange(
        stream,
        json!({
            "version": 1,
            "id": id,
            "method": "hello",
            "protocol": {"min": 1, "max": 1},
            "compression": ["none"],
            "capabilities": {
                "required": ["kv", "transactions"],
                "optional": ["prefix-list"]
            }
        }),
    )
}

#[test]
fn sidecar_negotiates_hello_compatibility_matrix() {
    let root = std::env::temp_dir().join(format!("ferrite-cli-handshake-{}", std::process::id()));
    let socket = root.with_extension("sock");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&socket);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "serve",
            root.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut compatible = UnixStream::connect(&socket).unwrap();
    let negotiated = exchange(
        &mut compatible,
        json!({
            "version": 1,
            "id": 1,
            "method": "hello",
            "protocol": {"min": 1, "max": 1},
            "compression": ["none"],
            "capabilities": {
                "required": ["kv", "transactions"],
                "optional": ["prefix-list", "future-capability"]
            }
        }),
    );
    assert_eq!(negotiated["ok"], true);
    assert_eq!(negotiated["result"]["protocol"], 1);
    assert_eq!(negotiated["result"]["compression"], "none");
    assert_eq!(
        negotiated["result"]["capabilities"],
        json!(["kv", "transactions", "prefix-list"])
    );

    let mut wider = UnixStream::connect(&socket).unwrap();
    let wider_negotiated = exchange(
        &mut wider,
        json!({
            "version": 1,
            "id": 2,
            "method": "hello",
            "protocol": {"min": 0, "max": 2},
            "compression": ["gzip", "none"],
            "capabilities": {"required": [], "optional": ["kv"]}
        }),
    );
    assert_eq!(wider_negotiated["ok"], true);
    assert_eq!(wider_negotiated["result"]["protocol"], 1);
    assert_eq!(wider_negotiated["result"]["compression"], "none");
    assert_eq!(wider_negotiated["result"]["capabilities"], json!(["kv"]));
    assert_eq!(
        exchange(
            &mut compatible,
            json!({"version":1,"id":2,"method":"get","key":"missing"}),
        )["ok"],
        true
    );

    let cases = [
        (
            json!({"version":1,"id":3,"method":"hello","protocol":{"min":2,"max":3},"compression":["none"],"capabilities":{"required":[],"optional":[]}}),
            "incompatible protocol versions",
        ),
        (
            json!({"version":1,"id":4,"method":"hello","protocol":{"min":1,"max":1},"compression":["gzip"],"capabilities":{"required":[],"optional":[]}}),
            "no mutually supported compression",
        ),
        (
            json!({"version":1,"id":5,"method":"hello","protocol":{"min":1,"max":1},"compression":["none"],"capabilities":{"required":["future-capability"],"optional":[]}}),
            "unsupported required capability: future-capability",
        ),
        (
            json!({"version":1,"id":6,"method":"hello","protocol":{"min":2,"max":1},"compression":["none"],"capabilities":{"required":[],"optional":[]}}),
            "incompatible protocol versions",
        ),
        (
            json!({"version":1,"id":7,"method":"hello","protocol":{"min":1,"max":1},"compression":[],"capabilities":{"required":[],"optional":[]}}),
            "no mutually supported compression",
        ),
        (
            json!({"version":1,"id":8,"method":"hello","compression":["none"],"capabilities":{"required":[],"optional":[]}}),
            "missing protocol range",
        ),
        (
            json!({"version":1,"id":9,"method":"hello","protocol":{"min":"1","max":1},"compression":["none"],"capabilities":{"required":[],"optional":[]}}),
            "missing protocol minimum",
        ),
        (
            json!({"version":1,"id":10,"method":"hello","protocol":{"min":1,"max":1},"compression":"none","capabilities":{"required":[],"optional":[]}}),
            "missing array compression",
        ),
        (
            json!({"version":1,"id":11,"method":"hello","protocol":{"min":1,"max":1},"compression":["none"],"capabilities":{"required":[1],"optional":[]}}),
            "required must contain strings",
        ),
        (
            json!({"version":1,"id":12,"method":"hello","protocol":{"min":1,"max":1},"compression":["none"]}),
            "missing capabilities",
        ),
    ];
    for (request, expected_error) in cases {
        let mut stream = UnixStream::connect(&socket).unwrap();
        let response = exchange(&mut stream, request);
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"], expected_error);
    }

    let mut retry = UnixStream::connect(&socket).unwrap();
    assert_eq!(
        exchange(
            &mut retry,
            json!({"version":1,"id":6,"method":"hello","protocol":{"min":2,"max":2},"compression":["none"],"capabilities":{"required":[],"optional":[]}}),
        )["ok"],
        false
    );
    assert_eq!(hello(&mut retry, 7)["ok"], true);
    let repeated = hello(&mut retry, 8);
    assert_eq!(repeated["ok"], false);
    assert_eq!(repeated["error"], "hello handshake already completed");

    let mut limited = UnixStream::connect(&socket).unwrap();
    assert_eq!(
        exchange(
            &mut limited,
            json!({"version":1,"id":9,"method":"hello","protocol":{"min":1,"max":1},"compression":["none"],"capabilities":{"required":["kv"],"optional":[]}}),
        )["ok"],
        true
    );
    let unavailable = exchange(&mut limited, json!({"version":1,"id":10,"method":"list"}));
    assert_eq!(unavailable["ok"], false);
    assert_eq!(
        unavailable["error"],
        "capability not negotiated: prefix-list"
    );

    let mut missing = UnixStream::connect(&socket).unwrap();
    let response = exchange(&mut missing, json!({"version":1,"id":11,"method":"list"}));
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"], "hello handshake required");

    child.kill().unwrap();
    child.wait().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    let _ = std::fs::remove_file(socket);
}

#[test]
fn sidecar_speaks_versioned_ndjson() {
    let root = std::env::temp_dir().join(format!("ferrite-cli-{}", std::process::id()));
    let socket = root.with_extension("sock");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&socket);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "serve",
            root.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut stream = UnixStream::connect(&socket).unwrap();
    assert_eq!(hello(&mut stream, 1)["ok"], true);
    writeln!(stream, "{}", json!({"version":1,"id":1,"method":"transaction","operations":[{"Put":{"key":"hello","value":{"world":true}}}]})).unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true);
    writeln!(
        stream,
        "{}",
        json!({"version":1,"id":2,"method":"get","key":"hello"})
    )
    .unwrap();
    line.clear();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"],
        json!({"world":true})
    );
    child.kill().unwrap();
    child.wait().unwrap();
    let db = ferrite_core::Database::open(&root).unwrap();
    assert_eq!(db.get("hello").unwrap(), Some(json!({"world":true})));
    std::fs::remove_dir_all(root).unwrap();
    let _ = std::fs::remove_file(socket);
}

#[test]
fn sidecar_rejects_protocol_mismatch_and_oversized_requests() {
    let root = std::env::temp_dir().join(format!("ferrite-cli-limits-{}", std::process::id()));
    let socket = root.with_extension("sock");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&socket);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "serve",
            root.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut stream = UnixStream::connect(&socket).unwrap();
    writeln!(
        stream,
        "{}",
        json!({"version":2,"id":7,"method":"get","key":"missing"})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["id"], 7);
    assert_eq!(response["error"], "unsupported protocol version");

    let mut oversized = UnixStream::connect(&socket).unwrap();
    oversized
        .write_all(&vec![b'x'; 2 * 1024 * 1024 + 1])
        .unwrap();
    oversized.write_all(b"\n").unwrap();
    let mut rejected = String::new();
    match BufReader::new(oversized).read_line(&mut rejected) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) => {}
        result => panic!("oversized request was not rejected: {result:?}"),
    }

    let mut healthy = UnixStream::connect(&socket).unwrap();
    assert_eq!(hello(&mut healthy, 8)["ok"], true);
    writeln!(healthy, "{}", json!({"version":1,"id":9,"method":"list"})).unwrap();
    line.clear();
    BufReader::new(healthy).read_line(&mut line).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["ok"], true);

    child.kill().unwrap();
    child.wait().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    let _ = std::fs::remove_file(socket);
}

#[test]
fn sidecar_preserves_a_pre_existing_socket_path() {
    let root = std::env::temp_dir().join(format!("ferrite-cli-stale-{}", std::process::id()));
    let socket = root.with_extension("sock");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&socket);
    std::fs::write(&socket, b"owned by user").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "serve",
            root.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("socket path already exists"));
    assert_eq!(std::fs::read(&socket).unwrap(), b"owned by user");

    std::fs::remove_file(socket).unwrap();
    if root.exists() {
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn idle_client_does_not_block_other_clients() {
    let root = std::env::temp_dir().join(format!("ferrite-cli-idle-{}", std::process::id()));
    let socket = root.with_extension("sock");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&socket);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "serve",
            root.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let idle = UnixStream::connect(&socket).unwrap();
    let mut healthy = UnixStream::connect(&socket).unwrap();
    healthy
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    assert_eq!(hello(&mut healthy, 9)["ok"], true);
    writeln!(healthy, "{}", json!({"version":1,"id":10,"method":"list"})).unwrap();
    let mut line = String::new();
    let result = BufReader::new(healthy).read_line(&mut line);

    drop(idle);
    child.kill().unwrap();
    child.wait().unwrap();
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(socket);

    result.expect("idle client blocked a healthy client");
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["ok"], true);
}

#[test]
fn serve_rejects_non_regular_schema_input() {
    let root =
        std::env::temp_dir().join(format!("ferrite-cli-schema-input-{}", std::process::id()));
    let socket = root.with_extension("sock");
    let schema = root.with_extension("schema-dir");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&schema);
    std::fs::create_dir(&schema).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "serve",
            root.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--schema",
            schema.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let _ = std::fs::remove_dir_all(&schema);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&root);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("schema must be a regular file"));
}

#[test]
fn sidecar_bounds_idle_connection_workers() {
    let root =
        std::env::temp_dir().join(format!("ferrite-cli-worker-limit-{}", std::process::id()));
    let socket = root.with_extension("sock");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&socket);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "serve",
            root.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut idle = Vec::new();
    for _ in 0..64 {
        idle.push(UnixStream::connect(&socket).unwrap());
    }
    let mut excess = UnixStream::connect(&socket).unwrap();
    excess
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    writeln!(excess, "{}", json!({"version":1,"id":10,"method":"list"})).unwrap();
    let mut line = String::new();
    let result = BufReader::new(excess).read_line(&mut line);

    drop(idle);
    child.kill().unwrap();
    child.wait().unwrap();
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(socket);

    match result {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) => {}
        other => panic!("excess connection was not rejected: {other:?}, response={line:?}"),
    }
}
