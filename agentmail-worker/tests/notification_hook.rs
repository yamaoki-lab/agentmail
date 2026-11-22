use agentmail_worker::{runtime::TemporaryDirectory, DATA_DIRECTORY};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const UUID: &str = "11111111-1111-1111-1111-111111111111";
const OTHER_UUID: &str = "22222222-2222-2222-2222-222222222222";

struct Fixture {
    directory: TemporaryDirectory,
    client: PathBuf,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let directory = TemporaryDirectory::new(&std::env::temp_dir(), "agentmail-hook-test.")
            .unwrap();
        let home = directory.path();
        let sessions = home.join(DATA_DIRECTORY).join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join(format!("claude-{UUID}")), "probe\n").unwrap();
        let client = home.join("client");
        let script = format!(
            "#!/bin/sh\ntest \"$1\" = --local-notifications && test \"$2\" = claude && test \"$3\" = {UUID} || exit 99\n: > \"$HOME/called\"\n{script}\n"
        );
        fs::write(&client, script).unwrap();
        fs::set_permissions(&client, fs::Permissions::from_mode(0o700)).unwrap();
        Self { directory, client }
    }

    fn home(&self) -> &Path {
        self.directory.path()
    }

    fn hook(&self, mode: &str, stop: bool, active: bool, uuid: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentmail-hook"));
        if stop {
            command.arg("--stop");
        }
        let mut child = command
            .args([mode])
            .arg(&self.client)
            .arg(self.home().join("state"))
            .env("HOME", self.home())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = json!({
            "session_id": uuid,
            "cwd": self.home(),
            "stop_hook_active": active,
        });
        write!(child.stdin.take().unwrap(), "{payload}").unwrap();
        child.wait_with_output().unwrap()
    }
}

fn assert_silent(output: Output) {
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[test]
fn claude_hook_silences_client_errors() {
    for script in [
        "printf '%s' 'unexpected output'; printf '%s' 'failed snapshot' >&2; exit 1",
        "printf '%s' 'failed snapshot' >&2; exit 0",
        "printf '%s' 'invalid JSON'; exit 0",
    ] {
        let fixture = Fixture::new(script);
        for mode in ["--claude-registered", "--claude-local"] {
            for stop in [false, true] {
                let called = fixture.home().join("called");
                assert_silent(fixture.hook(mode, stop, false, UUID));
                fs::remove_file(called).unwrap();
            }
        }
        assert!(!fixture.home().join("state").exists());
    }
}

#[test]
fn claude_hook_skips_unregistered_and_reentered_stop() {
    let fixture = Fixture::new("exit 1");
    assert_silent(fixture.hook("--claude-registered", false, false, OTHER_UUID));
    assert_silent(fixture.hook("--claude-registered", true, true, UUID));
    assert!(!fixture.home().join("called").exists());
}

#[test]
fn claude_hook_emits_only_metadata_and_shares_stop_state() {
    let snapshot = json!({
        "identity": "probe@agentmail.test",
        "uid_validity": 1,
        "unread_count": 1,
        "messages": [{
            "uid": 7,
            "sender": "sender@agentmail.test",
            "subject": "日本語の件名",
            "body": "BODY_MUST_NOT_APPEAR",
        }],
    });
    let fixture = Fixture::new(&format!("printf '%s' '{snapshot}'"));
    let output = fixture.hook("--claude-registered", true, false, UUID);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let notification: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(notification["decision"], "block");
    let reason = notification["reason"].as_str().unwrap();
    let metadata: Value = serde_json::from_str(reason.lines().rev().nth(1).unwrap()).unwrap();
    assert_eq!(
        metadata,
        json!({
            "unread_count": 1,
            "new_messages": [{"sender": "sender@agentmail.test", "subject": "日本語の件名"}],
        })
    );
    assert!(!reason.contains("BODY_MUST_NOT_APPEAR"));
    assert_silent(fixture.hook("--claude-registered", false, false, UUID));

    let fixture = Fixture::new(&format!("printf '%s' '{snapshot}'"));
    let output = fixture.hook("--claude-registered", false, false, UUID);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let notification: Value = serde_json::from_slice(&output.stdout).unwrap();
    let hook = &notification["hookSpecificOutput"];
    assert_eq!(hook["hookEventName"], "PostToolUse");
    assert!(hook["additionalContext"].as_str().unwrap().ends_with(&metadata.to_string()));
    assert_silent(fixture.hook("--claude-registered", true, false, UUID));
}
