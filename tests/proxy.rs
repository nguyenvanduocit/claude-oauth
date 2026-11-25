use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
use std::process::{Command, Stdio};

#[test]
fn proxy_preserves_native_invocation_and_selects_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("claude");
    fs::write(
        &claude,
        r#"#!/bin/sh
printf '%s\0' "$CLAUDE_CODE_OAUTH_TOKEN" "${ANTHROPIC_API_KEY-unset}" "$@"
cat
echo diagnostic >&2
exit 42
"#,
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
    let store = dir.path().join("accounts.json");
    for managed in [false, true] {
        if managed {
            fs::write(
                &store,
                r#"{"accounts":[{"name":"work","token":"fixture"}]}"#,
            )
            .unwrap();
        }
        let args = ["--help", "rm", "--", "", "a b", "--auto"].map(OsStr::new);
        let args = args.into_iter().chain([OsStr::from_bytes(b"x\xff")]);
        let mut child = Command::new(env!("CARGO_BIN_EXE_claude-oauth"))
            .arg("proxy")
            .args(args.clone())
            .env("CLAUDE_OAUTH_CLAUDE", &claude)
            .env("CLAUDE_OAUTH_HOME", dir.path())
            .env_remove("CLAUDE_OAUTH_ACCOUNT")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "native")
            .env("ANTHROPIC_API_KEY", "key")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"input").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(42));
        assert_eq!(out.stderr, b"diagnostic\n");
        let mut expected = if managed {
            b"fixture\0unset\0".to_vec()
        } else {
            b"native\0key\0".to_vec()
        };
        for arg in args {
            expected.extend_from_slice(arg.as_bytes());
            expected.push(0);
        }
        expected.extend_from_slice(b"input");
        assert_eq!(out.stdout, expected);
    }
}
