//! End-to-end checks of session monitoring with a fake `claude` that calls the hooks it is
//! given through `--settings`, the way Claude Code does.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
settings = None
if "--settings" in args:
    i = args.index("--settings")
    settings = json.loads(args[i + 1])
    args = args[:i] + args[i + 2:]
token = os.environ.get("CLAUDE_CODE_OAUTH_TOKEN", "")
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(json.dumps({"token": token, "args": args, "hooked": settings is not None}) + "\n")
behaviour = os.environ.get("FAKE_" + token.split("-")[-1].upper(), "ok")
def hook(event, payload):
    # Like Claude Code, keep the OAuth token out of the hook's environment.
    env = {k: v for k, v in os.environ.items() if k != "CLAUDE_CODE_OAUTH_TOKEN"}
    env["ANTHROPIC_AUTH_TOKEN"] = ""
    for group in settings["hooks"][event]:
        for entry in group["hooks"]:
            subprocess.run(entry["command"], shell=True, input=json.dumps(payload).encode(), env=env)
if behaviour == "silent-fail":
    sys.exit(1)
if settings is None:
    sys.exit(0)
if behaviour == "ok":
    hook("Stop", {"hook_event_name": "Stop", "session_id": "sess-ok"})
    sys.exit(0)
hook("StopFailure", {"hook_event_name": "StopFailure", "session_id": "sess-1", "error": behaviour,
                     "last_assistant_message": "You've hit your 5-hour limit"})
sys.exit(1)
"#;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        fs::write(&claude, FAKE).unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            dir.path().join("accounts.json"),
            r#"{"default":"alpha","accounts":[
                {"name":"alpha","token":"sk-ant-oat01-alpha"},
                {"name":"beta","token":"sk-ant-oat01-beta"}]}"#,
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn command(&self, api: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claude-oauth"));
        command
            .args(args)
            .env("CLAUDE_OAUTH_HOME", self.dir.path())
            .env("CLAUDE_OAUTH_CLAUDE", self.path("claude"))
            .env("CLAUDE_OAUTH_API_URL", api)
            .env("FAKE_LOG", self.path("log"))
            .env_remove("CLAUDE_OAUTH_ACCOUNT")
            .env_remove("CLAUDE_OAUTH_ON_LIMIT")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, api: &str, args: &[&str], behaviour: &[(&str, &str)]) -> Output {
        let mut command = self.command(api, args);
        command.envs(behaviour.iter().copied());
        command.output().unwrap()
    }

    /// The launches the fake recorded since the last call.
    fn launches(&self) -> Vec<serde_json::Value> {
        let text = fs::read_to_string(self.path("log")).unwrap_or_default();
        let _ = fs::remove_file(self.path("log"));
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn state(&self) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.path("state.json")).unwrap()).unwrap()
    }
}

/// Nothing listens here, so API calls fail fast as "offline".
const OFFLINE: &str = "http://127.0.0.1:9";

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A local API that answers every request with `status`.
fn api_answering(status: u16) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    format!("http://{address}")
}

fn tokens(launches: &[serde_json::Value]) -> Vec<&str> {
    launches
        .iter()
        .map(|launch| launch["token"].as_str().unwrap())
        .collect()
}

#[test]
fn a_limit_mid_session_moves_the_conversation_to_the_next_account() {
    let fixture = Fixture::new();
    let output = fixture.run(
        OFFLINE,
        &["run", "--on-limit", "switch", "--", "-p", "hello"],
        &[("FAKE_ALPHA", "rate_limit")],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let launches = fixture.launches();
    assert_eq!(
        tokens(&launches),
        ["sk-ant-oat01-alpha", "sk-ant-oat01-beta"]
    );
    assert_eq!(launches[0]["args"], serde_json::json!(["-p", "hello"]));
    assert_eq!(
        launches[1]["args"],
        serde_json::json!(["-p", "hello", "--resume", "sess-1"])
    );
    assert!(launches.iter().all(|launch| launch["hooked"] == true));
    let log = stderr(&output);
    assert!(
        log.contains("\"alpha\": usage limit reached, resets"),
        "{log}"
    );
    assert!(log.contains("continuing with \"beta\""), "{log}");

    // alpha now rests, so the next session starts on beta without being asked.
    let listed = fixture.run(OFFLINE, &["list"], &[]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("alpha  usage limit reached, resets"));
    let output = fixture.run(OFFLINE, &["run", "-q", "--", "-p", "again"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(tokens(&fixture.launches()), ["sk-ant-oat01-beta"]);
    assert!(stderr(&output).contains("\"alpha\": usage limit reached"));

    // Naming the resting account still uses it, with a warning.
    let output = fixture.run(OFFLINE, &["run", "-q", "-a", "alpha", "--", "-p", "x"], &[]);
    assert_eq!(tokens(&fixture.launches()), ["sk-ant-oat01-alpha"]);
    assert!(stderr(&output).contains("Using it because you asked for it"));
}

#[test]
fn stop_prints_how_to_continue_and_keeps_the_exit_code() {
    let fixture = Fixture::new();
    let output = fixture.run(
        OFFLINE,
        &["run", "--on-limit", "stop"],
        &[("FAKE_ALPHA", "rate_limit")],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(tokens(&fixture.launches()), ["sk-ant-oat01-alpha"]);
    assert!(
        stderr(&output).contains("claude-oauth run -a 'beta' -- '--resume' 'sess-1'"),
        "{}",
        stderr(&output)
    );
    // Without a terminal, "ask" behaves like "stop".
    let output = fixture.run(
        OFFLINE,
        &["run", "-a", "alpha"],
        &[("FAKE_ALPHA", "rate_limit")],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fixture.launches().len(), 1);
}

#[test]
fn an_auth_failure_marks_the_token_rejected() {
    let fixture = Fixture::new();
    let output = fixture.run(
        OFFLINE,
        &["run", "--on-limit", "switch"],
        &[("FAKE_ALPHA", "authentication_failed")],
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        tokens(&fixture.launches()),
        ["sk-ant-oat01-alpha", "sk-ant-oat01-beta"]
    );
    assert!(stderr(&output).contains("\"alpha\": token rejected"));
    let state = fixture.state();
    let entries: Vec<_> = state["tokens"].as_object().unwrap().values().collect();
    assert!(entries.iter().any(|entry| entry["health"] == "rejected"));
}

#[test]
fn a_failed_exit_without_a_hook_triggers_a_token_check() {
    let fixture = Fixture::new();
    // 401 from the API: the token is bad, so the session moves on.
    let output = fixture.run(
        &api_answering(401),
        &["run", "--on-limit", "switch"],
        &[("FAKE_ALPHA", "silent-fail")],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let launches = fixture.launches();
    assert_eq!(
        tokens(&launches),
        ["sk-ant-oat01-alpha", "sk-ant-oat01-beta"]
    );
    assert_eq!(launches[1]["args"], serde_json::json!([]));

    // Offline: nothing proves the account is at fault, so the exit code is passed through.
    let fixture = Fixture::new();
    let output = fixture.run(
        OFFLINE,
        &["run", "--on-limit", "switch"],
        &[("FAKE_ALPHA", "silent-fail")],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fixture.launches().len(), 1);
}

#[test]
fn claude_commands_and_proxy_sessions() {
    let fixture = Fixture::new();
    let output = fixture.run(OFFLINE, &["proxy", "mcp", "list"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(fixture.launches()[0]["hooked"], false);

    let output = fixture.run(
        OFFLINE,
        &["proxy", "-p", "hi"],
        &[
            ("FAKE_ALPHA", "rate_limit"),
            ("CLAUDE_OAUTH_ON_LIMIT", "switch"),
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let launches = fixture.launches();
    assert_eq!(
        tokens(&launches),
        ["sk-ant-oat01-alpha", "sk-ant-oat01-beta"]
    );
    assert!(launches[0]["hooked"] == true);
    // A successful turn clears a stale rest.
    let state = fixture.state();
    assert!(
        state["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "ok")
    );
}

/// A fake session that reports which signal reached it.
const SIGNALS: &str = r#"#!/usr/bin/env python3
import os, signal, sys, time
def caught(number, _frame):
    with open(os.environ["FAKE_LOG"], "a") as log:
        log.write(f"{number}\n")
    sys.exit(100 + number)
signal.signal(signal.SIGINT, caught)
signal.signal(signal.SIGTERM, caught)
open(os.environ["FAKE_LOG"] + ".ready", "w").close()
time.sleep(30)
sys.exit(0)
"#;

fn signalled(signal: i32, whole_group: bool) -> (Option<i32>, String) {
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::new();
    fs::write(fixture.path("claude"), SIGNALS).unwrap();
    let mut command = fixture.command(OFFLINE, &["run", "-q"]);
    // Its own process group, like a terminal job, so the whole group can be signalled.
    command.process_group(0);
    let mut child = command.spawn().unwrap();
    let ready = fixture.path("log.ready");
    let started = std::time::Instant::now();
    while !ready.exists() {
        assert!(
            started.elapsed().as_secs() < 10,
            "the fake session did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pid = child.id() as i32;
    // SAFETY: only sends a signal.
    unsafe {
        if whole_group {
            libc::killpg(pid, signal);
        } else {
            libc::kill(pid, signal);
        }
    }
    let status = child.wait().unwrap();
    (
        status.code(),
        fs::read_to_string(fixture.path("log")).unwrap_or_default(),
    )
}

#[test]
fn ctrl_c_reaches_claude_and_the_wrapper_waits_for_it() {
    // A terminal sends SIGINT to the whole foreground group: Claude Code handles it and the
    // wrapper exits with Claude Code's status instead of dying first.
    assert_eq!(
        signalled(libc::SIGINT, true),
        (Some(100 + libc::SIGINT), format!("{}\n", libc::SIGINT))
    );
}

#[test]
fn sigterm_to_the_wrapper_is_passed_on() {
    assert_eq!(
        signalled(libc::SIGTERM, false),
        (Some(100 + libc::SIGTERM), format!("{}\n", libc::SIGTERM))
    );
}

#[test]
fn a_session_that_ignores_sigterm_is_killed_after_the_grace_period() {
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::new();
    fs::write(
        fixture.path("claude"),
        "#!/usr/bin/env python3\nimport os, signal, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\n\
         open(os.environ['FAKE_LOG'] + '.ready', 'w').close()\ntime.sleep(60)\n",
    )
    .unwrap();
    let mut command = fixture.command(OFFLINE, &["run", "-q"]);
    command
        .env("CLAUDE_OAUTH_KILL_GRACE", "0.5")
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let ready = fixture.path("log.ready");
    let started = std::time::Instant::now();
    while !ready.exists() {
        assert!(
            started.elapsed().as_secs() < 10,
            "the fake session did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let signalled = std::time::Instant::now();
    // SAFETY: only sends a signal.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(128 + libc::SIGKILL));
    assert!(signalled.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn the_terminal_is_restored_after_claude_leaves_it_raw() {
    use nix::sys::termios::{LocalFlags, tcgetattr};
    let fixture = Fixture::new();
    fs::write(
        fixture.path("claude"),
        "#!/usr/bin/env python3\nimport sys, tty\ntty.setraw(sys.stdin.fileno())\n",
    )
    .unwrap();
    let pty = nix::pty::openpty(None, None).unwrap();
    let cooked = tcgetattr(&pty.slave).unwrap().local_flags;
    assert!(cooked.contains(LocalFlags::ICANON | LocalFlags::ECHO));
    let mut command = fixture.command(OFFLINE, &["run", "-q"]);
    command.stdin(Stdio::from(pty.slave.try_clone().unwrap()));
    let status = command.status().unwrap();
    assert_eq!(status.code(), Some(0));
    let after = tcgetattr(&pty.slave).unwrap().local_flags;
    assert!(
        after.contains(LocalFlags::ICANON | LocalFlags::ECHO),
        "{after:?}"
    );
}
