//! The local account store (`accounts.json`) and the last-known account state
//! (`state.json`), both owner-only files updated atomically under one lock.

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^sk-ant-oat[A-Za-z0-9_-]+$").unwrap());

pub fn valid_token(token: &str) -> bool {
    token.len() <= 8192 && TOKEN.is_match(token)
}

pub fn check_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        bail!("Account names must be 1-64 characters without control characters.");
    }
    Ok(name.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub name: String,
    pub token: String,
    #[serde(default)]
    pub paused: bool,
    /// Unix time Claude Code issued the token through `add`; unknown for pasted or restored tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Accounts {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

impl Accounts {
    pub fn find(&self, name: &str) -> Result<&Account> {
        self.accounts
            .iter()
            .find(|account| account.name == name)
            .with_context(|| {
                format!(
                    "No account named \"{name}\". Run `claude-oauth list` to see your accounts."
                )
            })
    }

    pub fn find_mut(&mut self, name: &str) -> Result<&mut Account> {
        self.accounts
            .iter_mut()
            .find(|account| account.name == name)
            .with_context(|| {
                format!(
                    "No account named \"{name}\". Run `claude-oauth list` to see your accounts."
                )
            })
    }

    pub fn has_name(&self, name: &str) -> bool {
        self.accounts.iter().any(|account| account.name == name)
    }

    pub fn has_token(&self, token: &str) -> Option<&Account> {
        self.accounts.iter().find(|account| account.token == token)
    }

    /// `name`, or `name (2)`, `name (3)`… when the name is taken.
    pub fn free_name(&self, name: &str) -> String {
        if !self.has_name(name) {
            return name.to_string();
        }
        (2..)
            .map(|n| format!("{name} ({n})"))
            .find(|candidate| !self.has_name(candidate))
            .unwrap()
    }

    /// The account `run` uses when none is named: the default, else the first one not paused.
    pub fn preferred(&self) -> Option<&Account> {
        self.default
            .as_deref()
            .and_then(|name| {
                self.accounts
                    .iter()
                    .find(|account| account.name == name && !account.paused)
            })
            .or_else(|| self.accounts.iter().find(|account| !account.paused))
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn locate() -> Result<Self> {
        let dir = if let Some(dir) =
            std::env::var_os("CLAUDE_OAUTH_HOME").filter(|value| !value.is_empty())
        {
            PathBuf::from(dir)
        } else if let Some(dir) =
            std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
        {
            PathBuf::from(dir).join("claude-oauth")
        } else {
            let home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .context("HOME is not set.")?;
            PathBuf::from(home).join(".config").join("claude-oauth")
        };
        Ok(Self { dir })
    }

    #[cfg(test)]
    pub fn at(dir: &std::path::Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("accounts.json")
    }

    fn ensure_dir(&self) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .with_context(|| format!("Could not create {}", self.dir.display()))
    }

    pub fn state_file(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn read(&self) -> Result<Accounts> {
        load(&self.file(), "account store")
    }

    pub fn read_state(&self) -> crate::state::State {
        // State is a cache: a damaged file only costs a fresh check.
        load(&self.state_file(), "state").unwrap_or_default()
    }

    fn locked<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.ensure_dir()?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.dir.join(".lock"))
            .context("Could not open the store lock.")?;
        let _lock = acquire(lock)?;
        work()
    }

    /// Read, change and atomically rewrite the account store while holding the lock.
    pub fn update<T>(&self, change: impl FnOnce(&mut Accounts) -> Result<T>) -> Result<T> {
        self.locked(|| {
            let mut accounts = self.read()?;
            let result = change(&mut accounts)?;
            if accounts
                .default
                .as_deref()
                .is_some_and(|name| !accounts.has_name(name))
            {
                accounts.default = None;
            }
            save(&self.file(), &accounts)?;
            Ok(result)
        })
    }

    /// Read, change and rewrite the state file while holding the lock.
    pub fn update_state<T>(&self, change: impl FnOnce(&mut crate::state::State) -> T) -> Result<T> {
        self.locked(|| {
            let mut state = self.read_state();
            let result = change(&mut state);
            state.prune(chrono::Utc::now().timestamp());
            save(&self.state_file(), &state)?;
            Ok(result)
        })
    }
}

/// Wait at most `LOCK_WAIT` for the store lock, so a stuck process can never hang a hook
/// (and with it a Claude Code session) or a command.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

fn acquire(file: File) -> Result<nix::fcntl::Flock<File>> {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    let mut file = file;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(lock),
            Err((returned, Errno::EWOULDBLOCK | Errno::EINTR))
                if std::time::Instant::now() < deadline =>
            {
                file = returned;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err((_, Errno::EWOULDBLOCK)) => bail!(
                "Another claude-oauth process has held the account store lock for over {} seconds.",
                LOCK_WAIT.as_secs()
            ),
            Err((_, error)) => return Err(error).context("Could not lock the account store."),
        }
    }
}

fn load<T: serde::de::DeserializeOwned + Default>(path: &Path, what: &str) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("The {what} at {} is not valid JSON.", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}

fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    // A randomly named 0600 file in the same directory, removed on drop unless persisted, so
    // an interrupted write never leaves a partial file in place or blocks the next one.
    let result = (|| {
        let directory = path.parent().unwrap_or(Path::new("."));
        let mut file = tempfile::Builder::new()
            .prefix(".claude-oauth-")
            .tempfile_in(directory)?;
        file.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        anyhow::Ok(())
    })();
    result.with_context(|| format!("Could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn account(name: &str, suffix: &str) -> Account {
        Account {
            name: name.into(),
            token: format!("sk-ant-oat01-{suffix}"),
            paused: false,
            created_at: None,
        }
    }

    #[test]
    fn writes_owner_only_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(&dir.path().join("nested"));
        store
            .update(|accounts| {
                accounts.accounts.push(account("work", "aaa"));
                accounts.default = Some("work".into());
                Ok(())
            })
            .unwrap();
        let mode = fs::metadata(store.file()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let read = store.read().unwrap();
        assert_eq!(read.accounts, vec![account("work", "aaa")]);
        assert_eq!(read.default.as_deref(), Some("work"));
    }

    #[test]
    fn gives_up_on_a_lock_held_too_long() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        store.update(|_| Ok(())).unwrap();
        let held = nix::fcntl::Flock::lock(
            File::open(dir.path().join(".lock")).unwrap(),
            nix::fcntl::FlockArg::LockExclusive,
        )
        .unwrap();
        let started = std::time::Instant::now();
        let error = store.update(|_| Ok(())).unwrap_err().to_string();
        assert!(error.contains("held the account store lock"), "{error}");
        assert!(started.elapsed() >= LOCK_WAIT);
        drop(held);
        store.update(|_| Ok(())).unwrap();
    }

    #[test]
    fn drops_a_default_that_was_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        store
            .update(|accounts| {
                accounts.accounts.push(account("work", "aaa"));
                accounts.default = Some("work".into());
                Ok(())
            })
            .unwrap();
        store
            .update(|accounts| {
                accounts.accounts.clear();
                Ok(())
            })
            .unwrap();
        assert_eq!(store.read().unwrap().default, None);
    }

    #[test]
    fn prefers_the_default_unless_paused() {
        let mut accounts = Accounts {
            default: Some("b".into()),
            accounts: vec![account("a", "a"), account("b", "b")],
        };
        assert_eq!(accounts.preferred().unwrap().name, "b");
        accounts.accounts[1].paused = true;
        assert_eq!(accounts.preferred().unwrap().name, "a");
        accounts.accounts[0].paused = true;
        assert!(accounts.preferred().is_none());
    }

    #[test]
    fn picks_free_names_and_validates_tokens() {
        let accounts = Accounts {
            default: None,
            accounts: vec![account("work", "a"), account("work (2)", "b")],
        };
        assert_eq!(accounts.free_name("work"), "work (3)");
        assert_eq!(accounts.free_name("home"), "home");
        assert!(valid_token("sk-ant-oat01-abc_DEF-123"));
        assert!(!valid_token("sk-ant-api03-abc"));
        assert!(!valid_token("sk-ant-oat01-abc def"));
        assert!(check_name("  ").is_err());
        assert_eq!(check_name(" Work ").unwrap(), "Work");
    }
}
