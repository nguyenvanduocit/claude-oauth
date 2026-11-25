//! Guided token creation: run Claude Code's official `claude setup-token` in a private
//! pseudo-terminal, hand the sign-in link to the user, type their authorization code back
//! into Claude Code and capture the token it prints.

use anyhow::{Context, Result, bail};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use process_wrap::std::{ChildWrapper, CommandWrap, KillOnDrop, ProcessSession, ResetSigmask};
use regex::Regex;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

static TERMINAL_CODES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1B(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1B]*(?:\x07|\x1B\\))").unwrap()
});
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https://[^\x00-\x20<>"']+"#).unwrap());
static TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^A-Za-z0-9_-])(sk-ant-oat[A-Za-z0-9_-]{20,})").unwrap());
static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)long-lived\s*authentication\s*token\s*created").unwrap());
static FOOTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)store\s*this\s*token\s*securely|use\s*this\s*token\s*by\s*setting").unwrap()
});
static CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\x21-\x22\x24-\x7e]+#[\x21-\x22\x24-\x7e]+$").unwrap());

pub const CODE_HELP: &str =
    "Paste the complete code from Claude, including the # and the characters after it.";

fn clean(output: &str) -> String {
    TERMINAL_CODES.replace_all(output, "").into_owned()
}

/// The first Anthropic sign-in link, once it is fully printed.
pub fn authorize_link(output: &str) -> Option<String> {
    let text = clean(output);
    LINK.find_iter(&text)
        .filter(|found| found.end() < text.len()) // still being printed otherwise
        .find_map(|found| {
            let url = url::Url::parse(found.as_str()).ok()?;
            let host = url.host_str()?;
            let first_party = url.scheme() == "https"
                && url.port().is_none()
                && ((host == "claude.com" && url.path() == "/cai/oauth/authorize")
                    || (["claude.ai", "console.anthropic.com", "platform.claude.com"]
                        .contains(&host)
                        && url.path() == "/oauth/authorize"));
            // Older Claude Code prints the PKCE parameters; newer releases print one opaque
            // request handle (`/oauth/authorize?<handle>`). Accept either, never a bare page.
            let query = |key: &str| {
                url.query_pairs()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.into_owned())
            };
            let pkce = ["state", "code_challenge", "client_id"]
                .iter()
                .all(|key| query(key).is_some_and(|value| !value.is_empty()))
                && query("response_type").as_deref() == Some("code");
            let handle = url.query().is_some_and(|query| {
                query.len() >= 16
                    && query
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            });
            let pkce = pkce || handle;
            (first_party && pkce).then(|| url.to_string())
        })
}

pub fn issued_token(output: &str) -> Option<String> {
    let text = clean(output);
    let token = TOKEN.captures(&text)?.get(1)?.as_str().to_string();
    crate::store::valid_token(&token).then_some(token)
}

/// The token printed after "Long-lived authentication token created", read only once the
/// instructions that follow it have arrived so a partial token is never captured.
pub fn announced_token(output: &str) -> Option<String> {
    let text = clean(output);
    let heading = HEADING.find(&text)?;
    let after = &text[heading.end()..];
    let footer = FOOTER.find(after)?;
    issued_token(&after[..footer.start()])
}

pub fn authorization_failure(output: &str) -> Option<&'static str> {
    // Ink's cursor moves can leave words visually separated without whitespace in the stream.
    let text: String = clean(output)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    if text.contains("includingthe#inthemiddle") || text.contains("doesn'tlooklikethefullcode") {
        return Some(CODE_HELP);
    }
    if text.contains("oautherror:") || text.contains("thatcodewasn'taccepted") {
        return Some(
            "Claude rejected this code. Run `claude-oauth add` again and use the code from the new sign-in.",
        );
    }
    None
}

pub fn complete_code(code: &str) -> bool {
    let code = code.trim();
    !code.starts_with("sk-ant-oat") && CODE.is_match(code)
}

/// Process-wide record of the running setup, so Ctrl-C can clean up.
/// The running setup's process group and profile, so a Ctrl-C at the code prompt (which
/// exits without unwinding) can still clean up.
static ACTIVE: Mutex<Option<(i32, PathBuf)>> = Mutex::new(None);

fn active() -> std::sync::MutexGuard<'static, Option<(i32, PathBuf)>> {
    ACTIVE.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Owns everything `claude setup-token` needs and tears it all down on drop: the process
/// session (every process in it is killed and reaped), the PTY reader thread and the
/// temporary profile.
struct Session {
    child: Box<dyn ChildWrapper>,
    reader: Option<std::thread::JoinHandle<()>>,
    _profile: tempfile::TempDir,
}

impl Drop for Session {
    fn drop(&mut self) {
        active().take();
        // SIGKILL to the whole session's process group, then reap.
        let _ = self.child.kill();
        // With every process holding the PTY gone, the reader sees EOF/EIO and ends.
        if let Some(reader) = self.reader.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        // `_profile` is removed when the TempDir drops, after the processes using it are gone.
    }
}

pub struct Options<'a> {
    pub claude: &'a Path,
    pub open_browser: bool,
}

/// Run the sign-in and return the new token. `ask_code` shows the link and returns the
/// authorization code the user pasted.
pub fn create_token(
    options: Options,
    mut ask_code: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let _ = ctrlc::set_handler(|| {
        if let Some((group, profile)) = active().take() {
            let _ = killpg(Pid::from_raw(group), Signal::SIGKILL);
            let _ = fs::remove_dir_all(profile);
        }
        eprintln!();
        std::process::exit(130);
    });

    // TempDir creates the directory with mode 0700 and removes it on drop.
    let profile = tempfile::Builder::new()
        .prefix("claude-oauth-")
        .tempdir()
        .context("Could not create a temporary Claude profile.")?;

    // A wide terminal keeps Claude Code from wrapping the sign-in link.
    let size = Winsize {
        ws_row: 60,
        ws_col: 640,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(Some(&size), None).context("Could not open a pseudo-terminal.")?;
    let mut command = Command::new(options.claude);
    command
        .arg("setup-token")
        .current_dir(profile.path())
        .env("TERM", "xterm-256color")
        .env("BROWSER", "/usr/bin/false")
        .env("CLAUDE_CONFIG_DIR", profile.path())
        .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", profile.path())
        .env("DISABLE_TELEMETRY", "1")
        .stdin(Stdio::from(pty.slave.try_clone()?))
        .stdout(Stdio::from(pty.slave.try_clone()?))
        .stderr(Stdio::from(pty.slave.try_clone()?));
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if key.starts_with("ANTHROPIC_") || key.starts_with("CLAUDE_CODE_") {
            command.env_remove(key.as_ref());
        }
    }
    // Its own session and process group, off our terminal; killed as a group on drop.
    let child = CommandWrap::from(command)
        .wrap(ProcessSession)
        .wrap(ResetSigmask)
        .wrap(KillOnDrop)
        .spawn()
        .with_context(|| format!("Could not start {}", options.claude.display()))?;
    drop(pty.slave);
    *active() = Some((child.id() as i32, profile.path().to_path_buf()));

    let mut master = File::from(pty.master);
    let mut reader = master
        .try_clone()
        .context("Could not read the pseudo-terminal.")?;
    let (sender, chunks) = mpsc::channel::<Vec<u8>>();
    let reader = std::thread::spawn(move || {
        let mut buffer = [0u8; 32 * 1024];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut session = Session {
        child,
        reader: Some(reader),
        _profile: profile,
    };

    // Keep raw bytes: a multi-byte character can be split across two reads.
    let mut raw: Vec<u8> = Vec::new();
    let receive = |raw: &mut Vec<u8>, until: Instant| -> Option<bool> {
        match chunks.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(chunk) => {
                raw.extend_from_slice(&chunk);
                if raw.len() > 128 * 1024 {
                    raw.drain(..raw.len() - 128 * 1024);
                }
                Some(true)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(false),
        }
    };
    let text = |raw: &[u8]| String::from_utf8_lossy(raw).into_owned();

    let deadline = Instant::now() + Duration::from_secs(60);
    let link = loop {
        if let Some(link) = authorize_link(&text(&raw)) {
            break link;
        }
        match receive(&mut raw, deadline) {
            Some(true) => {}
            Some(false) => bail!(
                "Claude Code exited before showing a sign-in link.{}",
                tail(&text(&raw))
            ),
            None => bail!(
                "Claude Code did not show a sign-in link within a minute.{}",
                tail(&text(&raw))
            ),
        }
    };

    if options.open_browser {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = Command::new(opener)
            .arg(&link)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    let code = ask_code(&link)?;
    if !complete_code(&code) {
        bail!(CODE_HELP);
    }
    while chunks.try_recv().is_ok() {}
    raw.clear();
    master
        .write_all(format!("{}\r", code.trim()).as_bytes())
        .context("Claude Code closed before reading the code.")?;
    master.flush()?;

    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let output = text(&raw);
        if let Some(token) = announced_token(&output) {
            return Ok(token);
        }
        if let Some(failure) = authorization_failure(&output) {
            bail!(failure);
        }
        match receive(&mut raw, deadline) {
            Some(true) => {}
            Some(false) => {
                let succeeded = session.child.wait().is_ok_and(|status| status.success());
                return match issued_token(&text(&raw)).filter(|_| succeeded) {
                    Some(token) => Ok(token),
                    None => {
                        bail!("Claude Code did not issue a token. Check the sign-in and try again.")
                    }
                };
            }
            None => bail!(
                "Claude did not finish authorizing within 45 seconds. Run `claude-oauth add` again."
            ),
        }
    }
}

fn tail(output: &str) -> String {
    let text = clean(output);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let last = &lines[lines.len().saturating_sub(5)..];
    if last.is_empty() {
        String::new()
    } else {
        format!("\nLast output:\n  {}", last.join("\n  "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const AUTHORIZE: &str = "https://claude.com/cai/oauth/authorize?state=fixture&client_id=fixture&response_type=code&code_challenge=fixture";

    #[test]
    fn accepts_only_anthropic_links_with_pkce() {
        assert_eq!(
            authorize_link(&format!("\x1b[33m{AUTHORIZE}\x1b[0m\n")).as_deref(),
            Some(AUTHORIZE)
        );
        assert_eq!(
            authorize_link(AUTHORIZE),
            None,
            "a link still being printed is not used"
        );
        assert_eq!(
            authorize_link(&format!(
                "{}\n",
                AUTHORIZE.replace("claude.com", "wrong.invalid")
            )),
            None
        );
        assert_eq!(
            authorize_link(&format!(
                "{}\n",
                AUTHORIZE.split("&code_challenge").next().unwrap()
            )),
            None
        );
        // Claude Code 2.1.3xx: an OSC 8 hyperlink to an opaque request handle.
        let opaque = "https://claude.ai/oauth/authorize?fixture_request_handle_0123456789abcdefXYZ";
        assert_eq!(
            authorize_link(&format!(
                "\x1b]8;id=1sph63i;{opaque}\x07\x1b[38;5;246m{opaque}\x1b[39m\x1b]8;;\x07\r\n"
            ))
            .as_deref(),
            Some(opaque)
        );
        assert_eq!(
            authorize_link("https://claude.ai/oauth/authorize?x\n"),
            None
        );
        assert_eq!(authorize_link("https://claude.ai/oauth/authorize\n"), None);
    }

    #[test]
    fn reads_the_token_only_after_the_footer() {
        let token = format!("sk-ant-oat01-test-{}", "a".repeat(35));
        let partial = format!("Long-lived authentication token created\n{}", &token[..30]);
        assert_eq!(announced_token(&partial), None);
        let full = format!(
            "Long-lived authentication token created\n\x1b[32m{token}\x1b[0m\nStore this token securely."
        );
        assert_eq!(announced_token(&full), Some(token.clone()));
        assert_eq!(
            issued_token(&format!("Token: {token}-\n")),
            Some(format!("{token}-"))
        );
    }

    #[test]
    fn recognises_codes_and_failures() {
        assert!(complete_code(" abc#def "));
        assert!(!complete_code("abcdef"));
        assert!(!complete_code("sk-ant-oat01-x#y"));
        assert_eq!(
            authorization_failure("OAuth  error: invalid_grant"),
            authorization_failure("oauth error:x")
        );
        assert!(authorization_failure("That code wasn't accepted").is_some());
        assert!(authorization_failure("Paste code here").is_none());
    }

    /// A fake `claude setup-token` that leaves a grandchild running and records its profile.
    fn leaky_fake(dir: &Path, token: &str) -> PathBuf {
        let claude = dir.join("fake-claude");
        let probe = dir.display();
        fs::write(
            &claude,
            format!(
                "#!/bin/sh\npwd > '{probe}/profile'\nsleep 300 &\necho $! > '{probe}/grandchild'\n\
                 echo '{AUTHORIZE}'\nread code\necho 'Long-lived authentication token created'\n\
                 echo '{token}'\necho 'Store this token securely.'\nsleep 300\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
        claude
    }

    fn gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if nix::sys::signal::kill(Pid::from_raw(pid), None).is_err() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn leaves_no_process_or_profile_behind() {
        let token = format!("sk-ant-oat01-fixture-{}", "q".repeat(40));
        for succeed in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let claude = leaky_fake(dir.path(), &token);
            let result = create_token(
                Options {
                    claude: &claude,
                    open_browser: false,
                },
                |_| {
                    if succeed {
                        Ok("fixture#state".into())
                    } else {
                        anyhow::bail!("cancelled")
                    }
                },
            );
            assert_eq!(result.is_ok(), succeed);
            let grandchild: i32 = fs::read_to_string(dir.path().join("grandchild"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(
                gone(grandchild),
                "the grandchild {grandchild} outlived the setup"
            );
            let profile = fs::read_to_string(dir.path().join("profile")).unwrap();
            assert!(
                !Path::new(profile.trim()).exists(),
                "{profile} was not removed"
            );
        }
    }

    #[test]
    fn drives_claude_setup_token_through_a_real_pty() {
        let folder = tempfile::tempdir().unwrap();
        let claude = folder.path().join("fake-claude");
        let token = format!("sk-ant-oat01-fixture-{}", "p".repeat(40));
        fs::write(
            &claude,
            format!(
                "#!/bin/sh\n[ -n \"$CLAUDE_CODE_OAUTH_TOKEN\" ] && exit 3\necho '{AUTHORIZE}'\nread code\n\
                 [ \"$code\" = 'fixture#state' ] || exit 2\necho 'Long-lived authentication token created'\n\
                 echo 'Your OAuth token (valid for 1 year):'\necho '{token}'\necho 'Store this token securely.'\nsleep 30\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
        let mut shown = None;
        let started = Instant::now();
        let result = create_token(
            Options {
                claude: &claude,
                open_browser: false,
            },
            |link| {
                shown = Some(link.to_string());
                Ok("fixture#state".into())
            },
        );
        assert_eq!(result.unwrap(), token);
        assert_eq!(shown.as_deref(), Some(AUTHORIZE));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returns without waiting for Claude Code to exit"
        );
    }
}
