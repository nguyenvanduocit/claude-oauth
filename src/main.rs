mod api;
mod backup;
mod hook;
mod launch;
mod setup;
mod state;
mod store;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use launch::{OnLimit, find_claude};
use state::Status;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use store::{Account, Store, check_name, valid_token};

/// Manage Claude Code OAuth tokens for several Claude accounts and run Claude Code with any of them.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Add an account: sign in with Claude Code, or paste an existing token with --paste
    Add {
        name: String,
        /// Paste a token you already have (read from stdin when it is not a terminal)
        #[arg(long)]
        paste: bool,
        /// Print the sign-in link without opening a browser
        #[arg(long)]
        no_browser: bool,
        /// Make this the default account
        #[arg(long)]
        default: bool,
    },
    /// List accounts (offline; use `check` for health and quota)
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Check whether tokens work and how much quota is left
    #[command(visible_alias = "status")]
    Check {
        /// Only this account
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Set the default account used by `run`
    Use { name: String },
    /// Run Claude Code with an account's token; arguments after the options go to `claude`
    #[command(trailing_var_arg = true)]
    Run {
        /// Account to use (default: $CLAUDE_OAUTH_ACCOUNT, then the default account)
        #[arg(short = 'a', long)]
        account: Option<String>,
        /// Pick the working account with the most quota left
        #[arg(long, conflicts_with = "account")]
        auto: bool,
        /// Do not print which account is used
        #[arg(short = 'q', long)]
        quiet: bool,
        /// When an account hits its limit or is rejected mid-session
        #[arg(long, value_enum, default_value = "ask")]
        on_limit: OnLimit,
        /// Arguments for claude
        #[arg(allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Forward every argument to Claude Code (for shell aliases)
    ///
    /// Chat sessions get the same monitoring as `run`; set CLAUDE_OAUTH_ON_LIMIT to
    /// ask, switch or stop to choose what happens when an account hits its limit.
    Proxy {
        #[arg(num_args = 0.., allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Print shell exports for an account: eval "$(claude-oauth env work)"
    Env { name: Option<String> },
    /// Rename an account
    Rename { name: String, new_name: String },
    /// Pause an account so `run` skips it
    Pause { name: String },
    /// Resume a paused account
    Resume { name: String },
    /// Remove an account
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Write an encrypted backup of all accounts
    Backup {
        /// Output file, or - for stdout (default: claude-oauth-YYYY-MM-DD.backup.json)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Overwrite the output file
        #[arg(long)]
        force: bool,
    },
    /// Restore accounts from an encrypted backup (claude-oauth or Relayline), or - for stdin
    Restore { file: PathBuf },
    /// Called by Claude Code hooks that `run` and `proxy` add to a session
    #[command(hide = true)]
    Hook { event: String },
}

fn main() {
    // The entire tail belongs to Claude, including its help flags and literal "--".
    let mut argv = std::env::args_os().skip(1);
    let cli = if argv.next().as_deref() == Some(std::ffi::OsStr::new("proxy")) {
        Cli {
            command: Some(Commands::Proxy {
                args: argv.collect(),
            }),
        }
    } else {
        Cli::parse()
    };
    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let store = Store::locate()?;
    match cli.command.unwrap_or(Commands::List { json: false }) {
        Commands::Add {
            name,
            paste,
            no_browser,
            default,
        } => add(&store, &name, paste, !no_browser, default),
        Commands::List { json } => list(&store, json),
        Commands::Check { name, json } => check(&store, name.as_deref(), json),
        Commands::Use { name } => {
            store.update(|accounts| {
                if accounts.find(&name)?.paused {
                    eprintln!(
                        "note: \"{name}\" is paused; `run` will skip it until you resume it."
                    );
                }
                accounts.default = Some(name.clone());
                Ok(())
            })?;
            eprintln!("Default account: {name}");
            Ok(())
        }
        Commands::Run {
            account,
            auto,
            quiet,
            on_limit,
            args,
        } => run_claude(&store, account, auto, quiet, on_limit, args),
        Commands::Proxy { args } => {
            let accounts = store.read()?;
            let name = selected_by_env();
            if accounts.accounts.is_empty() && name.is_none() {
                // No managed accounts: Claude Code's own login, untouched.
                return launch::exec(None, &args);
            }
            if !launch::is_session(&args) {
                return launch::exec(Some(pick(&accounts, name.as_deref())?), &args);
            }
            let state = store.read_state();
            let choice =
                launch::choose(&accounts, &state, name.as_deref(), &HashSet::new(), now())?;
            if let Some(note) = &choice.note {
                eprintln!("claude-oauth: {note}");
            }
            let options = launch::Session {
                on_limit: OnLimit::from_env(),
                quiet: true,
            };
            launch::session(&store, choice.account, args, options)
        }
        Commands::Hook { event } => {
            // A hook must never disturb Claude Code, whatever goes wrong here.
            let _ = hook::handle(&store, &event);
            Ok(())
        }
        Commands::Env { name } => {
            let accounts = store.read()?;
            let account = pick(&accounts, name.or_else(selected_by_env).as_deref())?;
            println!("export CLAUDE_CODE_OAUTH_TOKEN={}", quote(&account.token));
            println!("export CLAUDE_OAUTH_ACCOUNT={}", quote(&account.name));
            println!("unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN");
            Ok(())
        }
        Commands::Rename { name, new_name } => {
            let new_name = check_name(&new_name)?;
            store.update(|accounts| {
                if accounts.has_name(&new_name) {
                    bail!("An account named \"{new_name}\" already exists.");
                }
                accounts.find_mut(&name)?.name = new_name.clone();
                if accounts.default.as_deref() == Some(name.as_str()) {
                    accounts.default = Some(new_name.clone());
                }
                Ok(())
            })?;
            eprintln!("Renamed \"{name}\" to \"{new_name}\".");
            Ok(())
        }
        Commands::Pause { name } => set_paused(&store, &name, true),
        Commands::Resume { name } => set_paused(&store, &name, false),
        Commands::Remove { name, yes } => {
            store.read()?.find(&name)?;
            if !yes
                && !confirm(&format!(
                    "Remove \"{name}\" and its token from claude-oauth?"
                ))?
            {
                bail!("Cancelled.");
            }
            store.update(|accounts| {
                accounts.find(&name)?;
                accounts.accounts.retain(|account| account.name != name);
                Ok(())
            })?;
            eprintln!(
                "Removed \"{name}\". The token itself stays valid until you revoke it at claude.ai."
            );
            Ok(())
        }
        Commands::Backup { output, force } => backup_accounts(&store, output, force),
        Commands::Restore { file } => restore_accounts(&store, &file),
    }
}

fn add(
    store: &Store,
    name: &str,
    paste: bool,
    open_browser: bool,
    make_default: bool,
) -> Result<()> {
    let name = check_name(name)?;
    if store.read()?.has_name(&name) {
        bail!("An account named \"{name}\" already exists. Remove or rename it first.");
    }
    let created_at = (!paste).then(now);
    let token = if paste {
        let token = if std::io::stdin().is_terminal() {
            rpassword::prompt_password("Paste the OAuth token (input hidden): ")?
        } else {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        };
        let token = token.trim().to_string();
        if !valid_token(&token) {
            bail!(
                "That is not a Claude OAuth token (they start with sk-ant-oat). Create one with `claude setup-token`."
            );
        }
        token
    } else {
        let claude = find_claude()?;
        eprintln!("Starting `claude setup-token`…");
        setup::create_token(
            setup::Options {
                claude: &claude,
                open_browser,
            },
            |link| {
                if open_browser {
                    eprintln!(
                        "\nSign in to the Claude account for \"{name}\" in your browser. If it did not open, visit:"
                    );
                } else {
                    eprintln!("\nOpen this link and sign in to the Claude account for \"{name}\":");
                }
                eprintln!("\n  {link}\n");
                eprintln!("After you approve, Claude shows an authorization code.");
                loop {
                    let code = prompt("Paste the code here: ")?;
                    if setup::complete_code(&code) {
                        return Ok(code);
                    }
                    eprintln!("{}", setup::CODE_HELP);
                }
            },
        )?
    };

    if let Some(existing) = store.read()?.has_token(&token) {
        bail!("This token is already saved as \"{}\".", existing.name);
    }
    eprint!("Checking the token… ");
    let report = api::check(&token);
    let _ = store.update_state(|state| state.record_check(&token, &report, now()));
    eprintln!("{}", describe(&report));
    let (stored, is_default) = store.update(|accounts| {
        if let Some(existing) = accounts.has_token(&token) {
            bail!("This token is already saved as \"{}\".", existing.name);
        }
        let stored = accounts.free_name(&name);
        accounts.accounts.push(Account {
            name: stored.clone(),
            token: token.clone(),
            paused: false,
            created_at,
        });
        if make_default || accounts.default.is_none() {
            accounts.default = Some(stored.clone());
        }
        Ok((
            stored.clone(),
            accounts.default.as_deref() == Some(stored.as_str()),
        ))
    })?;
    eprintln!(
        "Saved \"{stored}\"{}.",
        if is_default {
            " as the default account"
        } else {
            ""
        }
    );
    if matches!(
        report.health,
        api::Health::Rejected | api::Health::Forbidden
    ) {
        eprintln!(
            "warning: Anthropic did not accept this token. Check it with `claude-oauth check {stored}`."
        );
    }
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Setup tokens are valid for a year; warn in the last month.
fn expiry_note(account: &Account, now: i64) -> Option<String> {
    let days = (account.created_at? + 365 * 86_400 - now) / 86_400;
    match days {
        ..0 => Some("token is over a year old and has probably expired".into()),
        0..=30 => Some(format!("token expires in about {days} days")),
        _ => None,
    }
}

fn list(store: &Store, json: bool) -> Result<()> {
    let accounts = store.read()?;
    let state = store.read_state();
    let now = now();
    if json {
        let rows: Vec<_> = accounts
            .accounts
            .iter()
            .map(|account| {
                let status = state.status(&account.token, now);
                let entry = state.token(&state::fingerprint(&account.token));
                serde_json::json!({
                    "name": account.name,
                    "default": accounts.default.as_deref() == Some(account.name.as_str()),
                    "paused": account.paused,
                    "status": status.short(),
                    "status_detail": status.describe(),
                    "resting_until": match status { Status::Resting { until, .. } => Some(until), _ => None },
                    "checked_at": entry.and_then(|entry| entry.checked_at),
                    "created_at": account.created_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if accounts.accounts.is_empty() {
        eprintln!("No accounts yet. Add one with `claude-oauth add <name>`.");
        eprintln!("Store: {}", store.file().display());
        return Ok(());
    }
    let width = accounts
        .accounts
        .iter()
        .map(|account| account.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    for account in &accounts.accounts {
        let marker = if accounts.default.as_deref() == Some(account.name.as_str()) {
            "*"
        } else {
            " "
        };
        let mut notes = Vec::new();
        if account.paused {
            notes.push("paused".to_string());
        }
        let status = state.status(&account.token, now);
        if status != Status::Unchecked && status != Status::Ready {
            notes.push(status.describe());
        }
        notes.extend(expiry_note(account, now));
        println!(
            "{}",
            format!("{marker} {:<width$}  {}", account.name, notes.join("; ")).trim_end()
        );
    }
    Ok(())
}

fn check(store: &Store, only: Option<&str>, json: bool) -> Result<()> {
    let accounts = store.read()?;
    let chosen: Vec<&Account> = match only {
        Some(name) => vec![accounts.find(name)?],
        None => accounts.accounts.iter().collect(),
    };
    if chosen.is_empty() {
        eprintln!("No accounts yet. Add one with `claude-oauth add <name>`.");
        return Ok(());
    }
    let reports = api::check_all(
        &chosen
            .iter()
            .map(|account| account.token.clone())
            .collect::<Vec<_>>(),
    );
    let now = now();
    let state = store.update_state(|state| {
        for (account, report) in chosen.iter().zip(&reports) {
            state.record_check(&account.token, report, now);
        }
        state.clone()
    })?;
    if json {
        let rows: Vec<_> = chosen
            .iter()
            .zip(&reports)
            .map(|(account, report)| {
                let status = state.status(&account.token, now);
                serde_json::json!({
                    "name": account.name,
                    "default": accounts.default.as_deref() == Some(account.name.as_str()),
                    "paused": account.paused,
                    "status": status.short(),
                    "status_detail": status.describe(),
                    "health": report.health,
                    "detail": report.detail,
                    "quota_status": report.quota_status,
                    "quota": report.quota,
                    "created_at": account.created_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let width = chosen
        .iter()
        .map(|account| account.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "  {:<width$}  {:<9}  {:>7}  {:>7}  NOTE",
        "NAME", "STATUS", "5H LEFT", "7D LEFT"
    );
    for (account, report) in chosen.iter().zip(&reports) {
        let marker = if accounts.default.as_deref() == Some(account.name.as_str()) {
            "*"
        } else {
            " "
        };
        let status = state.status(&account.token, now);
        let shown = if account.paused {
            "paused"
        } else {
            status.short()
        };
        let left = |id: &str| {
            report
                .quota
                .iter()
                .find(|window| window.id == id)
                .map(|window| format!("{:.0}%", 100.0 - window.used_percent))
                .unwrap_or("-".into())
        };
        let mut notes = Vec::new();
        match &status {
            Status::Ready | Status::Unchecked => {}
            other => notes.push(other.describe()),
        }
        if matches!(status, Status::Ready | Status::Resting { .. }) {
            match report.quota_status {
                api::QuotaStatus::RateLimited => {
                    notes.push("quota not readable right now (rate limited)".into())
                }
                api::QuotaStatus::Restricted => notes.push("quota not shown for this plan".into()),
                api::QuotaStatus::Unavailable => notes.push("quota unavailable".into()),
                api::QuotaStatus::Available => {
                    if status == Status::Ready
                        && let Some(at) = report
                            .quota
                            .iter()
                            .find(|window| window.id == "five_hour")
                            .and_then(|window| window.resets_at)
                    {
                        notes.push(format!("5h window resets {}", state::clock(at)));
                    }
                }
            }
        }
        notes.extend(expiry_note(account, now));
        println!(
            "{}",
            format!(
                "{marker} {:<width$}  {shown:<9}  {:>7}  {:>7}  {}",
                account.name,
                left("five_hour"),
                left("seven_day"),
                notes.join("; ")
            )
            .trim_end()
        );
    }
    Ok(())
}

fn describe(report: &api::Report) -> String {
    match report.health {
        api::Health::Ready => match report.quota.iter().find(|window| window.id == "five_hour") {
            Some(window) => format!(
                "ready ({:.0}% of the 5-hour quota left)",
                100.0 - window.used_percent
            ),
            None => "ready".into(),
        },
        api::Health::Rejected => "rejected by Anthropic".into(),
        api::Health::Forbidden => match &report.detail {
            Some(detail) => format!("blocked by Anthropic: {detail}"),
            None => "blocked by Anthropic".into(),
        },
        api::Health::RateLimited => "valid, but Anthropic is throttling it right now".into(),
        api::Health::Offline => "could not reach api.anthropic.com; saved without checking".into(),
    }
}

fn selected_by_env() -> Option<String> {
    std::env::var("CLAUDE_OAUTH_ACCOUNT")
        .ok()
        .filter(|name| !name.trim().is_empty())
}

/// The account for a command that is not a chat session: named, else the default.
fn pick<'a>(accounts: &'a store::Accounts, name: Option<&str>) -> Result<&'a Account> {
    if accounts.accounts.is_empty() {
        bail!("No accounts yet. Add one with `claude-oauth add <name>`.");
    }
    match name {
        Some(name) => {
            let account = accounts.find(name)?;
            if account.paused {
                bail!("\"{name}\" is paused. Resume it with `claude-oauth resume {name}`.");
            }
            Ok(account)
        }
        None => accounts
            .preferred()
            .context("Every account is paused. Resume one with `claude-oauth resume <name>`."),
    }
}

fn run_claude(
    store: &Store,
    name: Option<String>,
    auto: bool,
    quiet: bool,
    on_limit: OnLimit,
    args: Vec<OsString>,
) -> Result<()> {
    let accounts = store.read()?;
    let name = name.or_else(selected_by_env);
    if !launch::is_session(&args) {
        let account = pick(&accounts, name.as_deref())?;
        return launch::exec(Some(account), &args);
    }
    let account = if auto {
        let candidates: Vec<&Account> = accounts
            .accounts
            .iter()
            .filter(|account| !account.paused)
            .collect();
        if candidates.is_empty() {
            pick(&accounts, None)?;
        }
        let reports = api::check_all(
            &candidates
                .iter()
                .map(|account| account.token.clone())
                .collect::<Vec<_>>(),
        );
        let now = now();
        let state = store.update_state(|state| {
            for (account, report) in candidates.iter().zip(&reports) {
                state.record_check(&account.token, report, now);
            }
            state.clone()
        })?;
        let preferred = accounts.preferred().map(|account| account.name.clone());
        // Rank by the tighter quota window; unknown quota comes after any with room left,
        // and the default account wins ties.
        let rank = |account: &Account, report: &api::Report| {
            let used = ["five_hour", "seven_day"]
                .iter()
                .filter_map(|id| report.quota.iter().find(|window| window.id == *id))
                .map(|window| window.used_percent)
                .reduce(f64::max)
                .unwrap_or(99.5);
            (used, Some(&account.name) != preferred.as_ref())
        };
        candidates
            .iter()
            .zip(&reports)
            .filter(|(account, report)| {
                report.health == api::Health::Ready
                    && state.status(&account.token, now) == Status::Ready
            })
            .min_by(|left, right| {
                let (left, right) = (rank(left.0, left.1), rank(right.0, right.1));
                left.0.total_cmp(&right.0).then(left.1.cmp(&right.1))
            })
            .map(|(account, _)| *account)
            .context("No account is ready. Run `claude-oauth check` to see why.")?
    } else {
        let state = store.read_state();
        let choice = launch::choose(&accounts, &state, name.as_deref(), &HashSet::new(), now())?;
        if let Some(note) = &choice.note {
            eprintln!("claude-oauth: {note}");
        }
        choice.account
    };
    if !quiet {
        eprintln!("claude-oauth: using \"{}\"", account.name);
    }
    launch::session(store, account, args, launch::Session { on_limit, quiet })
}

fn set_paused(store: &Store, name: &str, paused: bool) -> Result<()> {
    store.update(|accounts| {
        accounts.find_mut(name)?.paused = paused;
        Ok(())
    })?;
    eprintln!("{} \"{name}\".", if paused { "Paused" } else { "Resumed" });
    Ok(())
}

fn backup_password(confirm_it: bool) -> Result<String> {
    if let Ok(password) = std::env::var("CLAUDE_OAUTH_BACKUP_PASSWORD") {
        backup::check_password(&password)?;
        return Ok(password);
    }
    let password = rpassword::prompt_password("Backup password: ")?;
    backup::check_password(&password)?;
    if confirm_it && rpassword::prompt_password("Confirm password: ")? != password {
        bail!("The passwords do not match.");
    }
    Ok(password)
}

fn backup_accounts(store: &Store, output: Option<PathBuf>, force: bool) -> Result<()> {
    let accounts = store.read()?;
    if accounts.accounts.is_empty() {
        bail!("There are no accounts to back up.");
    }
    let output = output.unwrap_or_else(|| {
        PathBuf::from(format!(
            "claude-oauth-{}.backup.json",
            chrono::Local::now().format("%Y-%m-%d")
        ))
    });
    let to_stdout = output.as_os_str() == "-";
    if !to_stdout && !force && output.exists() {
        bail!(
            "{} already exists. Choose another --output or pass --force.",
            output.display()
        );
    }
    eprintln!(
        "The backup holds {} account tokens. Its password must be at least {} characters.",
        accounts.accounts.len(),
        backup::MIN_PASSWORD
    );
    let password = zeroize::Zeroizing::new(backup_password(true)?);
    let entries: Vec<backup::Entry> = accounts
        .accounts
        .iter()
        .map(|account| backup::Entry {
            label: account.name.clone(),
            token: account.token.clone(),
            parked: account.paused,
        })
        .collect();
    let text = backup::encrypt(&entries, &password)?;
    if to_stdout {
        println!("{text}");
    } else {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .create_new(!force)
            .mode(0o600)
            .open(&output)
            .with_context(|| format!("Could not create {}", output.display()))?;
        writeln!(file, "{text}")?;
        eprintln!(
            "Wrote an encrypted backup of {} accounts to {}",
            entries.len(),
            output.display()
        );
    }
    Ok(())
}

fn restore_accounts(store: &Store, file: &Path) -> Result<()> {
    let mut text = String::new();
    if file.as_os_str() == "-" {
        std::io::stdin()
            .take(backup::MAX_BACKUP as u64 + 1)
            .read_to_string(&mut text)?;
    } else {
        std::fs::File::open(file)
            .with_context(|| format!("Could not open {}", file.display()))?
            .take(backup::MAX_BACKUP as u64 + 1)
            .read_to_string(&mut text)?;
    }
    let password = zeroize::Zeroizing::new(backup_password(false)?);
    let entries = backup::decrypt(&text, &password)?;
    let (added, skipped) = store.update(|accounts| {
        let mut added = Vec::new();
        let mut skipped = 0;
        for entry in &entries {
            if accounts.has_token(&entry.token).is_some() {
                skipped += 1;
                continue;
            }
            let name = accounts.free_name(entry.label.trim());
            accounts.accounts.push(Account {
                created_at: None,
                name: name.clone(),
                token: entry.token.clone(),
                paused: entry.parked,
            });
            added.push(name);
        }
        if accounts.default.is_none() {
            accounts.default = accounts
                .accounts
                .iter()
                .find(|account| !account.paused)
                .map(|account| account.name.clone());
        }
        Ok((added, skipped))
    })?;
    for name in &added {
        eprintln!("  + {name}");
    }
    eprintln!(
        "Restored {} accounts; skipped {skipped} already saved.",
        added.len()
    );
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    eprint!("{message}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("Cancelled.");
    }
    Ok(line.trim().to_string())
}

fn confirm(message: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("Pass --yes to confirm when not running in a terminal.");
    }
    Ok(matches!(
        prompt(&format!("{message} [y/N] "))?
            .to_lowercase()
            .as_str(),
        "y" | "yes"
    ))
}

/// Ask a yes/no question on the terminal; Enter means yes.
fn ask_yes(message: &str) -> bool {
    matches!(
        prompt(&format!("{message} [Y/n] "))
            .map(|answer| answer.to_lowercase())
            .as_deref(),
        Ok("" | "y" | "yes")
    )
}

/// Single-quote a value for POSIX shells.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}
