use assert_cmd::Command;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

fn check_request(profile: &str, override_url: bool, expected_effort: Option<&str>) {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let config = root.path().join("config.toml");
    std::fs::write(&config, profile.replace("ENDPOINT", &url)).unwrap();
    std::fs::write(root.path().join("README.md"), "Fixture implementation.\n").unwrap();
    let accepts_effort = expected_effort.is_some();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no API request received");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        assert!(length < 1_000_000);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        let rejected = !accepts_effort && request.get("reasoning_effort").is_some();
        let (status, response) = if rejected {
            (
                "400 Bad Request",
                json!({"error": "unknown field: reasoning_effort"}),
            )
        } else {
            (
                "200 OK",
                json!({"choices": [{"message": {"content":
                "<final_answer>\nREADME.md:1-1 (implementation)\n</final_answer>"}}]}),
            )
        };
        let response = response.to_string();
        write!(reader.get_mut(), "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        request
    });
    let mut command = Command::cargo_bin("repotracer").unwrap();
    command
        .arg("--config")
        .arg(config)
        .arg("--root")
        .arg(root.path())
        .env_remove("REPOTRACER_API_KEY")
        .env("REPOTRACER_NO_UPDATE", "1")
        .timeout(Duration::from_secs(10));
    if override_url {
        command.args(["--base-url", &url]);
    }
    let output = command
        .args(["scout", "where is the implementation?"])
        .output()
        .unwrap();
    let request = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}; reasoning_effort: {:?}, temperature: {:?}",
        String::from_utf8_lossy(&output.stderr),
        request.get("reasoning_effort"),
        request.get("temperature")
    );
    assert_eq!(
        request.get("reasoning_effort").and_then(Value::as_str),
        expected_effort
    );
    if expected_effort.is_some() {
        assert!(request.get("temperature").is_none());
    } else {
        assert_eq!(request["temperature"], 0.5);
    }
}

#[test]
fn legacy_compatible_profile_does_not_enable_reasoning() {
    check_request("[model]\nbackend = 'openai-compatible'\nmodel = 'custom-model'\nbase_url = 'ENDPOINT'\ntemperature = 0.5\n", false, None);
}

#[test]
fn base_url_override_does_not_inherit_native_reasoning() {
    check_request(
        "[model]\nmodel = 'custom-model'\ntemperature = 0.5\n",
        true,
        None,
    );
    check_request(
        "[model]\nmodel = 'custom-model'\nreasoning_effort = 'high'\ntemperature = 0.5\n",
        true,
        None,
    );
}

#[test]
fn compatible_profile_preserves_explicit_reasoning() {
    for override_url in [false, true] {
        check_request("[model]\nbackend = 'openai-compatible'\nmodel = 'custom-model'\nbase_url = 'ENDPOINT'\nreasoning_effort = 'medium'\n", override_url, Some("medium"));
    }
}
