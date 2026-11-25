//! Starting Claude Code for `run` and `proxy`.
//!
//! A chat session runs as a child process with two hooks added through `--settings`, so
//! claude-oauth learns when a turn hits a usage limit or an auth failure. When that happens
//! it can carry the conversation over to the next account with `--resume`. Claude Code's own
//! commands (`claude mcp …`, `--help`, …) are started with plain `exec`.

use crate::state::{self, State, fingerprint};
use crate::store::{Account, Accounts, Store};
use crate::{api, hook, quote};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::IsTerminal;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, sigaction};
use nix::sys::termios::{SetArg, Termios, tcgetattr, tcsetattr};
use nix::unistd::{Pid, tcgetpgrp, tcsetpgrp};
use process_wrap::std::{ChildWrapper, CommandWrap, KillOnDrop, ResetSigmask};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OnLimit {
    /// Ask in a terminal, otherwise stop
    Ask,
    /// Continue with the next ready account without asking
    Switch,
    /// Stop and print how to continue
    Stop,
}

impl OnLimit {
    /// `$CLAUDE_OAUTH_ON_LIMIT`, for `proxy`, which takes no options of its own.
    pub fn from_env() -> Self {
        match std::env::var("CLAUDE_OAUTH_ON_LIMIT").as_deref() {
            Ok("switch") => OnLimit::Switch,
            Ok("stop") => OnLimit::Stop,
            _ => OnLimit::Ask,
        }
    }
}

/// Claude Code commands that are not chat sessions (from `claude --help`, 2.1.x).
const COMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "config",
    "doctor",
    "gateway",
    "help",
    "import",
    "install",
    "kill",
    "logs",
    "mcp",
    "migrate-installer",
    "plugin",
    "plugins",
    "purge",
    "respawn",
    "rm",
    "sessions",
    "setup-token",
    "stop",
    "ultrareview",
    "update",
    "upgrade",
];

/// Whether these arguments start a chat session, the only kind of launch that is monitored.
pub fn is_session(args: &[OsString]) -> bool {
    let words: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let flag = |names: &[&str]| {
        words
            .iter()
            .any(|word| names.iter().any(|name| word == name))
    };
    if flag(&["-h", "--help", "-v", "--version"]) {
        return false;
    }
    // The user's own --settings must not be overridden by a second one.
    if words
        .iter()
        .any(|word| *word == "--settings" || word.to_string_lossy().starts_with("--settings="))
    {
        return false;
    }
    match words
        .iter()
        .find(|word| !word.to_string_lossy().starts_with('-'))
    {
        Some(first) => !COMMANDS.iter().any(|command| first == command),
        None => true,
    }
}

/// Arguments that continue `session` after a switch. A `-p` run repeats its prompt in the
/// resumed session, because the failed turn produced no answer. An interactive session reopens
/// the conversation as it was. Without a session the failure came before any turn, so the
/// same command simply runs again.
pub fn relaunch_args(args: &[OsString], session: Option<&str>) -> Vec<OsString> {
    let Some(id) = session else {
        return args.to_vec();
    };
    let print = args.iter().any(|arg| arg == "-p" || arg == "--print");
    let resume: Vec<OsString> = vec!["--resume".into(), id.into()];
    if !print {
        return resume;
    }
    let mut kept = Vec::new();
    let mut words = args.iter().peekable();
    while let Some(word) = words.next() {
        let text = word.to_string_lossy();
        if text == "-c" || text == "--continue" || text == "--fork-session" {
            continue;
        }
        if text == "-r" || text == "--resume" || text == "--session-id" {
            if words
                .peek()
                .is_some_and(|next| !next.to_string_lossy().starts_with('-'))
            {
                words.next();
            }
            continue;
        }
        if text.starts_with("--resume=") || text.starts_with("--session-id=") {
            continue;
        }
        kept.push(word.clone());
    }
    kept.extend(resume);
    kept
}

/// Accounts in the order `run` tries them: the default first, then the store order.
fn ordered(accounts: &Accounts) -> Vec<&Account> {
    let preferred = accounts.preferred().map(|account| account.name.as_str());
    let mut list: Vec<&Account> = accounts
        .accounts
        .iter()
        .filter(|account| !account.paused)
        .collect();
    list.sort_by_key(|account| Some(account.name.as_str()) != preferred);
    list
}

pub struct Choice<'a> {
    pub account: &'a Account,
    /// Why another account was picked, or a warning about the chosen one.
    pub note: Option<String>,
}

/// Pick the account for a session: the named one, or the first usable one in order.
pub fn choose<'a>(
    accounts: &'a Accounts,
    state: &State,
    named: Option<&str>,
    skip: &HashSet<String>,
    now: i64,
) -> Result<Choice<'a>> {
    if accounts.accounts.is_empty() {
        bail!("No accounts yet. Add one with `claude-oauth add <name>`.");
    }
    if let Some(name) = named {
        let account = accounts.find(name)?;
        if account.paused {
            bail!("\"{name}\" is paused. Resume it with `claude-oauth resume {name}`.");
        }
        let status = state.status(&account.token, now);
        let note = (!status.usable()).then(|| {
            format!(
                "\"{name}\": {}. Using it because you asked for it.",
                status.describe()
            )
        });
        return Ok(Choice { account, note });
    }
    let candidates: Vec<&Account> = ordered(accounts)
        .into_iter()
        .filter(|account| !skip.contains(&fingerprint(&account.token)))
        .collect();
    if candidates.is_empty() {
        bail!("Every account is paused. Resume one with `claude-oauth resume <name>`.");
    }
    let first = candidates[0];
    if let Some(account) = candidates
        .iter()
        .find(|account| state.status(&account.token, now).usable())
    {
        let note = (account.name != first.name).then(|| {
            format!(
                "\"{}\": {}. Using \"{}\".",
                first.name,
                state.status(&first.token, now).describe(),
                account.name
            )
        });
        return Ok(Choice { account, note });
    }
    let reasons: Vec<String> = candidates
        .iter()
        .map(|account| {
            format!(
                "  {}: {}",
                account.name,
                state.status(&account.token, now).describe()
            )
        })
        .collect();
    bail!(
        "No account is ready right now:\n{}\nRun `claude-oauth check` to refresh, or choose one with -a.",
        reasons.join("\n")
    )
}

/// `$CLAUDE_OAUTH_CLAUDE`, else the first `claude` on PATH that is not this program.
pub fn find_claude() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CLAUDE_OAUTH_CLAUDE").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let me = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .ok();
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("claude"))
        .find(|candidate| is_executable(candidate) && candidate.canonicalize().ok().as_ref() != me.as_ref())
        .context("Claude Code was not found on PATH. Install it, or set CLAUDE_OAUTH_CLAUDE to its path.")
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn with_account(command: &mut Command, account: &Account) {
    command
        .env("CLAUDE_CODE_OAUTH_TOKEN", &account.token)
        .env("CLAUDE_OAUTH_ACCOUNT", &account.name)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");
}

/// Replace this process with Claude Code.
pub fn exec(account: Option<&Account>, args: &[OsString]) -> Result<()> {
    let claude = find_claude()?;
    let mut command = Command::new(&claude);
    if let Some(account) = account {
        with_account(&mut command, account);
    }
    let error = command.args(args).exec();
    Err(error).with_context(|| format!("Could not start {}", claude.display()))
}

/// PID of the running Claude Code session, 0 when none.
static CHILD: AtomicI32 = AtomicI32::new(0);

/// How long Claude Code gets to exit after a forwarded SIGTERM/SIGHUP before SIGKILL:
/// 10 seconds, or `$CLAUDE_OAUTH_KILL_GRACE` seconds.
fn grace() -> Duration {
    let seconds = std::env::var("CLAUDE_OAUTH_KILL_GRACE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(10.0);
    Duration::from_secs_f64(seconds)
}

/// Signal dispositions and terminal state that claude-oauth changes while Claude Code runs.
/// Dropping it restores them, so a crashed session cannot leave the terminal raw or the
/// wrapper deaf to Ctrl-C.
struct Foreground {
    interrupt: Option<SigAction>,
    quit: Option<SigAction>,
    terminal: Option<Termios>,
    /// The terminal's foreground process group, when it is our controlling terminal.
    group: Option<Pid>,
}

impl Foreground {
    /// Save the terminal and hand Ctrl-C (SIGINT, SIGQUIT) to Claude Code: the terminal
    /// sends them to the whole foreground group, and the wrapper must outlive the session.
    fn enter() -> Self {
        let stdin = std::io::stdin();
        let (terminal, group) = if stdin.is_terminal() {
            (tcgetattr(&stdin).ok(), tcgetpgrp(&stdin).ok())
        } else {
            (None, None)
        };
        let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        // SAFETY: installs SIG_IGN, which runs no code in this process.
        let (interrupt, quit) = unsafe {
            (
                sigaction(Signal::SIGINT, &ignore).ok(),
                sigaction(Signal::SIGQUIT, &ignore).ok(),
            )
        };
        Self {
            interrupt,
            quit,
            terminal,
            group,
        }
    }

    /// The dispositions Claude Code should start with: whatever this process inherited.
    fn child_dispositions(&self) -> (libc::sighandler_t, libc::sighandler_t) {
        let inherited = |action: &Option<SigAction>| match action.map(|action| action.handler()) {
            Some(SigHandler::SigIgn) => libc::SIG_IGN,
            _ => libc::SIG_DFL,
        };
        (inherited(&self.interrupt), inherited(&self.quit))
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        // SAFETY: restores the dispositions saved in `enter`.
        unsafe {
            if let Some(action) = &self.interrupt {
                let _ = sigaction(Signal::SIGINT, action);
            }
            if let Some(action) = &self.quit {
                let _ = sigaction(Signal::SIGQUIT, action);
            }
        }
        let stdin = std::io::stdin();
        // If the session moved the terminal to another process group, take it back. Doing so
        // from the background raises SIGTTOU, so ignore that while it happens.
        if let Some(group) = &self.group
            && tcgetpgrp(&stdin).ok() != Some(*group)
        {
            let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
            // SAFETY: temporarily installs SIG_IGN and then restores the previous action.
            unsafe {
                if let Ok(previous) = sigaction(Signal::SIGTTOU, &ignore) {
                    let _ = tcsetpgrp(&stdin, *group);
                    let _ = sigaction(Signal::SIGTTOU, &previous);
                }
            }
        }
        if let Some(termios) = &self.terminal {
            let _ = tcsetattr(&stdin, SetArg::TCSANOW, termios);
        }
    }
}

/// Pass SIGTERM and SIGHUP on to Claude Code, and SIGKILL it if it is still running after
/// the grace period. With no session running, exit as those signals would.
fn forward_signals() {
    let grace = grace();
    let Ok(mut signals) = signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGHUP]) else {
        return;
    };
    std::thread::spawn(move || {
        for number in signals.forever() {
            let pid = CHILD.load(Ordering::SeqCst);
            if pid == 0 {
                std::process::exit(128 + number);
            }
            if let Ok(signal) = Signal::try_from(number) {
                let _ = kill(Pid::from_raw(pid), signal);
            }
            std::thread::spawn(move || {
                std::thread::sleep(grace);
                if CHILD.load(Ordering::SeqCst) == pid {
                    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                }
            });
        }
    });
}

/// Start Claude Code in this process group (it needs the terminal), killed and reaped if
/// claude-oauth bails out early. On Linux the kernel also sends it SIGTERM if claude-oauth
/// itself dies.
fn spawn(mut command: Command, foreground: &Foreground) -> std::io::Result<Box<dyn ChildWrapper>> {
    let (interrupt, quit) = foreground.child_dispositions();
    // SAFETY: signal() is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            libc::signal(libc::SIGINT, interrupt);
            libc::signal(libc::SIGQUIT, quit);
            Ok(())
        });
    }
    let mut wrapped = CommandWrap::from(command);
    wrapped.wrap(ResetSigmask);
    #[cfg(target_os = "linux")]
    wrapped.wrap(process_wrap::std::PropagateThreadDeath::default());
    wrapped.wrap(KillOnDrop);
    wrapped.spawn()
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

fn run_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Why the session ended, if the account was the problem.
fn account_problem(
    store: &Store,
    account: &Account,
    run: &str,
    code: i32,
) -> Option<Option<String>> {
    let state = store.read_state();
    match state.last_event(run) {
        Some(event) if state::account_failure(&event.kind) => Some(event.session_id.clone()),
        Some(_) => None,
        // Claude Code reports some failures (an invalid token, for one) before any turn runs,
        // so no hook fires. A failed exit without a hook event gets a direct token check.
        None if code != 0 => {
            // Short timeout: this runs while the user waits for the shell prompt.
            let report = api::health(&account.token, std::time::Duration::from_secs(3));
            let status = store
                .update_state(|state| {
                    state.record_check(&account.token, &report, now());
                    state.status(&account.token, now())
                })
                .ok()?;
            (!status.usable()).then_some(None)
        }
        None => None,
    }
}

pub struct Session {
    pub on_limit: OnLimit,
    pub quiet: bool,
}

/// Run Claude Code with `account` and keep it going across account failures. Never returns
/// normally: the process exits with Claude Code's status.
pub fn session(
    store: &Store,
    account: &Account,
    args: Vec<OsString>,
    options: Session,
) -> Result<()> {
    let claude = find_claude()?;
    let exe = std::env::current_exe().context("Could not find the claude-oauth executable.")?;
    let settings = hook::settings(&exe);
    forward_signals();
    let mut account = account.clone();
    let mut args = args;
    let mut tried = HashSet::new();
    loop {
        let run = run_id();
        let mut command = Command::new(&claude);
        command
            .arg("--settings")
            .arg(&settings)
            .args(&args)
            .env(hook::RUN_ENV, &run)
            .env(hook::TOKEN_ENV, fingerprint(&account.token));
        with_account(&mut command, &account);
        let status = {
            let foreground = Foreground::enter();
            let mut child = spawn(command, &foreground)
                .with_context(|| format!("Could not start {}", claude.display()))?;
            CHILD.store(child.id() as i32, Ordering::SeqCst);
            let status = child.wait();
            CHILD.store(0, Ordering::SeqCst);
            status
            // `foreground` restores signals and the terminal here, before any prompt.
        };
        let code = exit_code(status?);
        tried.insert(fingerprint(&account.token));

        let Some(session_id) = account_problem(store, &account, &run, code) else {
            std::process::exit(code);
        };
        let state = store.read_state();
        eprintln!(
            "claude-oauth: \"{}\": {}.",
            account.name,
            state.status(&account.token, now()).describe()
        );
        let accounts = store.read()?;
        let Ok(next) = choose(&accounts, &state, None, &tried, now()) else {
            eprintln!(
                "claude-oauth: no other account is ready. `claude-oauth check` shows when one will be."
            );
            std::process::exit(code);
        };
        let next = next.account.clone();
        let next_args = relaunch_args(&args, session_id.as_deref());
        let switch = match options.on_limit {
            OnLimit::Switch => true,
            OnLimit::Stop => false,
            OnLimit::Ask => {
                std::io::stdin().is_terminal()
                    && std::io::stderr().is_terminal()
                    && crate::ask_yes(&format!(
                        "Continue this conversation with \"{}\"?",
                        next.name
                    ))
            }
        };
        if !switch {
            let shown: Vec<String> = next_args
                .iter()
                .map(|arg| quote(&arg.to_string_lossy()))
                .collect();
            eprintln!(
                "claude-oauth: to continue with \"{}\": claude-oauth run -a {} -- {}",
                next.name,
                quote(&next.name),
                shown.join(" ")
            );
            std::process::exit(code);
        }
        if !options.quiet {
            eprintln!("claude-oauth: continuing with \"{}\"", next.name);
        }
        account = next;
        args = next_args;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Health, QuotaStatus, Report, Window};

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn monitors_sessions_but_not_claude_commands() {
        assert!(is_session(&args(&[])));
        assert!(is_session(&args(&["-p", "fix the mcp server"])));
        assert!(is_session(&args(&["--model", "opus", "-c"])));
        assert!(!is_session(&args(&["mcp", "list"])));
        assert!(!is_session(&args(&["--debug", "plugin", "install", "x"])));
        assert!(!is_session(&args(&["-p", "hi", "--help"])));
        assert!(!is_session(&args(&["--settings", "{}", "-p", "hi"])));
        assert!(!is_session(&args(&["--settings={}"])));
    }

    #[test]
    fn relaunches_print_runs_with_their_prompt_and_sessions_bare() {
        assert_eq!(
            relaunch_args(&args(&["--model", "opus"]), Some("s1")),
            args(&["--resume", "s1"])
        );
        assert_eq!(
            relaunch_args(&args(&["-p", "--resume", "old", "-c", "hello"]), Some("s1")),
            args(&["-p", "hello", "--resume", "s1"])
        );
        assert_eq!(
            relaunch_args(&args(&["-p", "hi"]), None),
            args(&["-p", "hi"])
        );
    }

    fn account(name: &str) -> Account {
        Account {
            name: name.into(),
            token: format!("sk-ant-oat01-{name}"),
            paused: false,
            created_at: None,
        }
    }

    #[test]
    fn skips_accounts_at_their_limit() {
        let accounts = Accounts {
            default: Some("b".into()),
            accounts: vec![account("a"), account("b"), account("c")],
        };
        let mut state = State::default();
        let now = 1_800_000_000;
        let full = Report {
            health: Health::Ready,
            detail: None,
            quota_status: QuotaStatus::Available,
            quota: vec![Window {
                id: "five_hour".into(),
                used_percent: 100.0,
                resets_at: Some(now + 60),
            }],
        };
        state.record_check("sk-ant-oat01-b", &full, now);
        let choice = choose(&accounts, &state, None, &HashSet::new(), now).unwrap();
        assert_eq!(choice.account.name, "a");
        assert!(
            choice
                .note
                .unwrap()
                .starts_with("\"b\": 5-hour limit reached")
        );
        let named = choose(&accounts, &state, Some("b"), &HashSet::new(), now).unwrap();
        assert_eq!(named.account.name, "b");
        assert!(named.note.is_some());
        let skip: HashSet<String> = ["a", "c"]
            .iter()
            .map(|name| fingerprint(&format!("sk-ant-oat01-{name}")))
            .collect();
        let error = choose(&accounts, &state, None, &skip, now)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("No account is ready"), "{error}");
        assert!(
            choose(&accounts, &state, None, &HashSet::new(), now + 61)
                .unwrap()
                .note
                .is_none()
        );
    }
}
