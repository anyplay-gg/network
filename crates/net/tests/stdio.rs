//! The built native service, through the launcher's real JSON-line boundary. No cloud or audio
//! hardware is substituted for a success: this test checks startup, signed-out commands and EOF.
//! Network sign-in, tunnels and voice use the loopback servers in the other integration tests.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_native_service_answers_the_launcher_and_exits_when_its_input_closes() {
    let data = tempfile::tempdir().unwrap();
    let mut service = Service(
        Command::new(env!("CARGO_BIN_EXE_anyplay-net"))
            .arg("--data")
            .arg(data.path())
            .arg("--file-credentials")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = service.0.stdin.take().unwrap();
    let output = service.0.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if send
                .send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    {
        let mut answer = |command: Value| {
            writeln!(input, "{command}").unwrap();
            input.flush().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let value = receive
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("service did not answer");
                if value["id"] == command["id"] {
                    break value;
                }
            }
        };
        // The first action must not start in a pre-initialized service.
        let before = answer(json!({"id": 1, "do": "add_friend", "handle": "test"}));
        assert_eq!(before["error"]["code"], "bad_state");
        let hello = answer(
            json!({"id": 2, "do": "hello", "launcher": "0.2.6", "cloud": "https://cloud.invalid"}),
        );
        let hello: anyplay_control::Answer = serde_json::from_value(hello).unwrap();
        let hello = hello.into_result::<anyplay_control::Hello>().unwrap();
        assert_eq!(hello.protocol, anyplay_control::VERSION);
        let signed_out = answer(json!({"id": 3, "do": "add_friend", "handle": "test"}));
        assert_eq!(signed_out["error"]["code"], "not_signed_in");
        let duplicate = answer(
            json!({"id": 4, "do": "hello", "launcher": "0.2.6", "cloud": "https://other.invalid"}),
        );
        assert_eq!(duplicate["error"]["code"], "bad_state");
    }
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = service.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "service did not exit after EOF");
        std::thread::yield_now();
    }
    reader.join().unwrap();
}
