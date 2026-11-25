mod api;
mod backup;
mod setup;
mod store;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
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
        /// Arguments for claude
        #[arg(allow_hyphen_values = true)]
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
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
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
            args,
        } => run_claude(&store, account, auto, quiet, args),
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
    if report.health == api::Health::Rejected {
        eprintln!(
            "warning: Anthropic rejected this token. Check it with `claude-oauth check {stored}`."
        );
    }
    Ok(())
}

fn list(store: &Store, json: bool) -> Result<()> {
    let accounts = store.read()?;
    if json {
        let rows: Vec<_> = accounts
            .accounts
            .iter()
            .map(|account| {
                serde_json::json!({
                    "name": account.name,
                    "default": accounts.default.as_deref() == Some(account.name.as_str()),
                    "paused": account.paused,
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
        let state = if account.paused { "paused" } else { "" };
        println!(
            "{}",
            format!("{marker} {:<width$}  {state}", account.name).trim_end()
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
    if json {
        let rows: Vec<_> = chosen
            .iter()
            .zip(&reports)
            .map(|(account, report)| {
                serde_json::json!({
                    "name": account.name,
                    "default": accounts.default.as_deref() == Some(account.name.as_str()),
                    "paused": account.paused,
                    "health": report.health,
                    "quota_status": report.quota_status,
                    "quota": report.quota,
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
        "  {:<width$}  {:<11}  {:>7}  {:>7}  5H RESETS",
        "NAME", "STATUS", "5H LEFT", "7D LEFT"
    );
    for (account, report) in chosen.iter().zip(&reports) {
        let marker = if accounts.default.as_deref() == Some(account.name.as_str()) {
            "*"
        } else {
            " "
        };
        let status = match (account.paused, report.health) {
            (true, api::Health::Ready) => "paused",
            (_, api::Health::Ready) => "ready",
            (_, api::Health::Rejected) => "rejected",
            (_, api::Health::Unreachable) => "unreachable",
        };
        let left = |id| {
            report
                .window(id)
                .map(|window| format!("{:.0}%", 100.0 - window.used_percent))
                .unwrap_or("-".into())
        };
        let resets = match (report.health, report.quota_status) {
            (api::Health::Ready, api::QuotaStatus::RateLimited) => {
                "quota: rate limited, try later".into()
            }
            (api::Health::Ready, api::QuotaStatus::Restricted) => {
                "quota: not shown for this plan".into()
            }
            (api::Health::Ready, api::QuotaStatus::Unavailable) => "quota: unavailable".into(),
            _ => report
                .window("five_hour")
                .and_then(|window| window.resets_at.as_deref())
                .map(relative)
                .unwrap_or_default(),
        };
        println!(
            "{marker} {:<width$}  {status:<11}  {:>7}  {:>7}  {resets}",
            account.name,
            left("five_hour"),
            left("seven_day")
        );
    }
    Ok(())
}

fn describe(report: &api::Report) -> String {
    match report.health {
        api::Health::Ready => match report.window("five_hour") {
            Some(window) => format!(
                "ready ({:.0}% of the 5-hour quota left)",
                100.0 - window.used_percent
            ),
            None => "ready".into(),
        },
        api::Health::Rejected => "rejected by Anthropic".into(),
        api::Health::Unreachable => {
            "could not reach api.anthropic.com; saved without checking".into()
        }
    }
}

fn relative(time: &str) -> String {
    let Ok(when) = chrono::DateTime::parse_from_rfc3339(time) else {
        return String::new();
    };
    let minutes = (when.timestamp() - chrono::Utc::now().timestamp()).max(0) / 60;
    match minutes {
        0 => "now".into(),
        1..60 => format!("in {minutes}m"),
        _ => format!("in {}h{:02}m", minutes / 60, minutes % 60),
    }
}

fn selected_by_env() -> Option<String> {
    std::env::var("CLAUDE_OAUTH_ACCOUNT")
        .ok()
        .filter(|name| !name.trim().is_empty())
}

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
    args: Vec<OsString>,
) -> Result<()> {
    let accounts = store.read()?;
    let account = if auto {
        let preferred = accounts.preferred().map(|account| account.name.clone());
        let mut candidates: Vec<&Account> = accounts
            .accounts
            .iter()
            .filter(|account| !account.paused)
            .collect();
        if candidates.is_empty() {
            pick(&accounts, None)?;
        }
        // Keep the default first so it wins ties.
        candidates.sort_by_key(|account| Some(&account.name) != preferred.as_ref());
        let reports = api::check_all(
            &candidates
                .iter()
                .map(|account| account.token.clone())
                .collect::<Vec<_>>(),
        );
        candidates
            .iter()
            .zip(&reports)
            .filter(|(_, report)| report.health == api::Health::Ready)
            // Accounts whose quota could not be read rank after any with quota left.
            .min_by(|left, right| {
                let rank = |report: &api::Report| report.pressure().unwrap_or(99.5);
                rank(left.1).total_cmp(&rank(right.1))
            })
            .map(|(account, _)| *account)
            .context("No account is ready. Run `claude-oauth check` to see why.")?
    } else {
        pick(&accounts, name.or_else(selected_by_env).as_deref())?
    };
    let claude = find_claude()?;
    if !quiet {
        eprintln!("claude-oauth: using \"{}\"", account.name);
    }
    let error = Command::new(&claude)
        .args(args)
        .env("CLAUDE_CODE_OAUTH_TOKEN", &account.token)
        .env("CLAUDE_OAUTH_ACCOUNT", &account.name)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .exec();
    Err(error).with_context(|| format!("Could not start {}", claude.display()))
}

/// `$CLAUDE_OAUTH_CLAUDE`, else the first `claude` on PATH that is not this program.
fn find_claude() -> Result<PathBuf> {
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
        .find(|candidate| {
            is_executable(candidate) && candidate.canonicalize().ok().as_ref() != me.as_ref()
        })
        .context("Claude Code was not found on PATH. Install it, or set CLAUDE_OAUTH_CLAUDE to its path.")
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
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

/// Single-quote a value for POSIX shells.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}
