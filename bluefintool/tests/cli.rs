use std::process::{Command, Stdio};
use std::time::Duration;

/// Returns the path to the bluefintool binary built by `cargo test`.
fn bin() -> std::path::PathBuf {
    // `cargo test` puts the binary in the same target dir as the test binary.
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("parent of deps dir")
        .to_path_buf();
    path.push("bluefintool");
    path
}

/// Pick a port unlikely to collide across parallel test runs.
/// Each test calls this with a different `offset`.
fn test_port(offset: u16) -> u16 {
    18000 + (std::process::id() % 500) as u16 + offset
}

/// Spawns a child, waits up to `dur` for it to exit, then kills it.
/// Returns (stdout, stderr) as strings.
fn wait_or_kill(
    mut child: std::process::Child,
    dur: Duration,
) -> (String, String) {
    let start = std::time::Instant::now();
    loop {
        if let Some(_status) = child.try_wait().expect("try_wait") {
            break;
        }
        if start.elapsed() > dur {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().expect("wait_with_output");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let output = Command::new(bin())
        .arg("--help")
        .output()
        .expect("failed to run bluefintool");

    assert!(output.status.success(), "exit code should be 0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("USAGE:"), "should print usage: {stderr}");
    assert!(stderr.contains("--listen"), "should mention --listen: {stderr}");
    assert!(stderr.contains("--diagnostics"), "should mention --diagnostics: {stderr}");
}

#[test]
fn no_args_prints_usage_and_exits_zero() {
    let output = Command::new(bin())
        .output()
        .expect("failed to run bluefintool");

    assert!(output.status.success(), "exit code should be 0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("USAGE:"), "should print usage: {stderr}");
}

#[test]
fn bad_flag_exits_nonzero() {
    let output = Command::new(bin())
        .arg("--bogus")
        .output()
        .expect("failed to run bluefintool");

    assert!(!output.status.success(), "should exit non-zero on unknown flag");
}

#[test]
fn listen_and_connect_transfer_data() {
    use std::io::Write;

    let port = test_port(0);
    let port_str = port.to_string();

    // Use -d on both sides so we get byte-count output to assert on.
    let mut server = Command::new(bin())
        .args(["-l", &port_str, "-d"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn server");

    std::thread::sleep(Duration::from_millis(500));

    let mut client = Command::new(bin())
        .args(["127.0.0.1", &port_str, "-d"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn client");

    let payload = b"test payload\n";
    {
        let stdin = client.stdin.as_mut().expect("client stdin");
        stdin.write_all(payload).expect("write to client");
    }
    drop(client.stdin.take());

    let (_client_stdout, client_stderr) = wait_or_kill(client, Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(500));
    let (_server_stdout, server_stderr) = wait_or_kill(server, Duration::from_secs(1));

    // Both sides should establish a connection.
    assert!(
        server_stderr.contains("connection established"),
        "server should report connection: {server_stderr}"
    );
    assert!(
        client_stderr.contains("connection established"),
        "client should report connection: {client_stderr}"
    );

    // Client should report sending exactly 13 bytes ("test payload\n").
    assert!(
        client_stderr.contains("sent 13 bytes"),
        "client should report sent 13 bytes: {client_stderr}"
    );

    // Client should show the data-sent event.
    assert!(
        client_stderr.contains("data-sent:"),
        "client should show data-sent event: {client_stderr}"
    );

    // Server should report receiving 13 bytes.
    assert!(
        server_stderr.contains("recv 13 bytes"),
        "server should report recv 13 bytes: {server_stderr}"
    );

    // Server hex dump should contain the payload bytes.
    // "test payload\n" starts with 't' = 0x74, 'e' = 0x65, 's' = 0x73, 't' = 0x74
    assert!(
        server_stderr.contains("74 65 73 74 20 70 61 79 6c 6f 61 64 0a"),
        "server hex dump should contain payload: {server_stderr}"
    );

    // Server should show the data-recv event.
    assert!(
        server_stderr.contains("data-recv:"),
        "server should show data-recv event: {server_stderr}"
    );
}

#[test]
fn diagnostics_mode_shows_connection_info() {
    use std::io::Write;

    let port = test_port(100);
    let port_str = port.to_string();

    let mut server = Command::new(bin())
        .args(["-l", &port_str, "-d"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn server");

    std::thread::sleep(Duration::from_millis(500));

    let mut client = Command::new(bin())
        .args(["127.0.0.1", &port_str, "-d"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn client");

    let payload = b"diag test\n";
    {
        let stdin = client.stdin.as_mut().expect("client stdin");
        stdin.write_all(payload).expect("write");
    }
    drop(client.stdin.take());

    let (_client_stdout, client_stderr) = wait_or_kill(client, Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(500));
    let (_server_stdout, server_stderr) = wait_or_kill(server, Duration::from_secs(1));

    // Client should show connection info with protocol fields.
    assert!(client_stderr.contains("version"), "client diag: version: {client_stderr}");
    assert!(client_stderr.contains("encrypted"), "client diag: encrypted: {client_stderr}");
    assert!(client_stderr.contains("mask"), "client diag: mask: {client_stderr}");
    assert!(client_stderr.contains("role"), "client diag: role: {client_stderr}");

    // Server should show connection info.
    assert!(server_stderr.contains("version"), "server diag: version: {server_stderr}");
    assert!(server_stderr.contains("server (listener)"), "server diag: role: {server_stderr}");

    // Client should report sending exactly 10 bytes ("diag test\n").
    assert!(
        client_stderr.contains("sent 10 bytes"),
        "client should report sent 10 bytes: {client_stderr}"
    );

    // Client should show hex dump with >>> prefix.
    assert!(client_stderr.contains(">>>"), "client diag: send hex: {client_stderr}");

    // Server should report receiving 10 bytes.
    assert!(
        server_stderr.contains("recv 10 bytes"),
        "server should report recv 10 bytes: {server_stderr}"
    );

    // Server should show hex dump with <<< prefix.
    assert!(server_stderr.contains("<<<"), "server diag: recv hex: {server_stderr}");

    // Server hex dump should contain the payload.
    // "diag test\n" = 64 69 61 67 20 74 65 73 74 0a
    assert!(
        server_stderr.contains("64 69 61 67 20 74 65 73 74 0a"),
        "server hex dump should contain 'diag test' bytes: {server_stderr}"
    );
}
