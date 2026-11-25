# claude-oauth

A small CLI that wraps [Claude Code](https://docs.claude.com/en/docs/claude-code) to manage OAuth tokens for several Claude accounts and to start Claude Code with whichever account you choose.

```sh
claude-oauth add work          # sign in through Claude Code's official setup-token flow
claude-oauth add personal
claude-oauth check             # which tokens work and how much quota is left
claude-oauth run -- -p "hi"    # run claude with the default account
claude-oauth run -a personal   # …or a specific one
claude-oauth run --auto        # …or the working account with the most quota left
```

When an account hits its usage limit or its token stops working in the middle of a session, claude-oauth notices, remembers it, and offers to carry the conversation over to the next ready account.

## Install

```sh
brew install nguyenvanduocit/tap/claude-oauth
```

Prebuilt binaries for macOS (Apple Silicon and Intel) and Linux (x86_64 and arm64, static) are attached to every [release](https://github.com/nguyenvanduocit/claude-oauth/releases). To build from source:

```sh
cargo install --git https://github.com/nguyenvanduocit/claude-oauth
```

It needs macOS or Linux, and `claude` on your `PATH` (or set `CLAUDE_OAUTH_CLAUDE` to its path).

## Adding accounts

`claude-oauth add <name>` runs `claude setup-token` in a private pseudo-terminal with a temporary Claude profile. It opens the Anthropic sign-in page (`--no-browser` only prints the link). Approve it, paste the authorization code that Claude shows, and the CLI saves the long-lived token that Claude Code issues. The temporary profile is deleted afterwards, and your normal Claude Code login is left untouched.

If you already have a token from `claude setup-token`, use `claude-oauth add <name> --paste`. The CLI asks for it with input hidden, or reads it from stdin when stdin is not a terminal.

Every new token is checked against `api.anthropic.com` before it is saved. The first account you add becomes the default.

## Commands

| Command | What it does |
| --- | --- |
| `add <name> [--paste] [--no-browser] [--default]` | Add an account |
| `list` (`ls`) `[--json]` | List accounts with their last known state; `*` marks the default. Works offline. |
| `check` (`status`) `[name] [--json]` | Check tokens now: health, quota left in the 5-hour and 7-day windows, reset times |
| `use <name>` | Set the default account |
| `run [-a name \| --auto] [-q] [--on-limit ask\|switch\|stop] [-- claude args…]` | Run `claude` with the account's token, monitored (see below) |
| `proxy [claude args…]` | Forward the entire argument list to Claude Code, for a shell alias. Uses native credentials until you add an account. |
| `env [name]` | Print shell exports: `eval "$(claude-oauth env work)"` |
| `rename <name> <new>` | Rename an account |
| `pause <name>` / `resume <name>` | `run` skips paused accounts |
| `remove` (`rm`) `<name> [-y]` | Forget an account. The token itself stays valid until you revoke it at claude.ai. |
| `backup [-o file\|-] [--force]` | Write a password-encrypted backup |
| `restore <file\|->` | Restore from a backup and skip tokens that are already saved |

`run` uses the account named with `-a` or `$CLAUDE_OAUTH_ACCOUNT`, even if it is at its limit (with a warning). Otherwise it takes the default account and then the others in order, skipping paused accounts and any that are known to be at their limit, rejected or blocked. `--auto` checks every account first and picks the working one whose tighter quota window has the most room. Accounts whose quota cannot be read are tried after those with quota left. Claude Code starts with `CLAUDE_CODE_OAUTH_TOKEN` set and `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` cleared, so the chosen account is the one being billed. Everything else, including your Claude Code settings, history and MCP servers, stays the same.

## Monitoring: limits, rejected tokens and switching

For a chat session, `run` and `proxy` start Claude Code as a child process and add two [hooks](https://docs.claude.com/en/docs/claude-code/hooks) with `--settings`. Your own settings and hooks still apply. Claude Code's other commands (`claude mcp …`, `--help`, `--version`), and sessions where you pass your own `--settings`, are started with plain `exec` and are not monitored.

- **`StopFailure`** runs when a turn ends with an API error. claude-oauth records:
  - `rate_limit`: the account is at its limit. It reads the usage windows to learn when the limit lifts, and the account is skipped until then. If usage cannot be read, the account rests for an hour, or six hours for a weekly limit.
  - `authentication_failed`: the token is rejected (expired or revoked).
  - `billing_error`, `account_on_hold`, `oauth_org_not_allowed`, `verification_required`: the account is blocked.
  - Other errors, such as `overloaded` or `server_error`, are temporary and change nothing.
- **`Stop`** runs after every turn that finishes normally, and marks the account as working again.
- Claude Code rejects an invalid token before any turn starts, so no hook runs. When Claude Code exits with an error and no hook reported anything, claude-oauth checks the token directly, with a 3-second limit.

When a session ends because of its account, claude-oauth says why. If another account is ready, `--on-limit` decides what happens next:

| `--on-limit` | |
| --- | --- |
| `ask` (default) | In a terminal, ask whether to continue with the next account. Without a terminal, behave like `stop`. |
| `switch` | Continue with the next account without asking. |
| `stop` | Exit with Claude Code's status and print the command that continues. |

Switching reopens the same conversation with `claude --resume <session>` on the next account. An interactive session reopens as it was; options from the first launch, such as `--model`, are not repeated. A `-p` run is repeated with its original arguments in the resumed session, because the failed turn produced no answer. If the failure came before any turn started, the same command runs again with the next account. `proxy` takes no options of its own, so set `CLAUDE_OAUTH_ON_LIMIT=ask|switch|stop` instead.

### Process handling

claude-oauth uses [`process-wrap`](https://crates.io/crates/process-wrap), `nix` and `signal-hook` so that no process outlives the job it belongs to and the terminal is always handed back:

- A monitored session runs in claude-oauth's own process group, so Claude Code keeps the terminal. Ctrl-C reaches Claude Code as usual. claude-oauth ignores it, waits for Claude Code to exit, and exits with its status. Claude Code starts with the signal dispositions and signal mask that claude-oauth inherited.
- SIGTERM and SIGHUP sent to claude-oauth are passed on to Claude Code. If it is still running 10 seconds later (`CLAUDE_OAUTH_KILL_GRACE` sets the number of seconds), it is killed with SIGKILL.
- The terminal settings and foreground process group are saved before Claude Code starts and restored when it exits, even if it crashed in raw mode.
- If claude-oauth itself fails after starting Claude Code, Claude Code is killed and reaped. On Linux, the kernel also sends Claude Code SIGTERM if claude-oauth dies (`PR_SET_PDEATHSIG`). macOS has no such mechanism, so after a `kill -9` of claude-oauth, Claude Code continues until the terminal closes.
- `add` runs `claude setup-token` in a separate session and process group on a private pseudo-terminal. When `add` finishes, fails or is interrupted, the whole group is killed and reaped, the reader thread ends, and the temporary profile is deleted.
- The account store and `state.json` are written through randomly named temporary files and renamed into place. The lock is waited for at most 5 seconds, so a hook never hangs a Claude Code session. claude-oauth exits with Claude Code's exit status.

`list` shows what was last learned, without network access. `check` asks Anthropic again and updates it. Tokens created with `add` are valid for a year, so `list` and `check` warn during the last 30 days.

## Use Claude Code through an alias

Add this line to `~/.zshrc` or `~/.bashrc` and open a new shell. For a local checkout, install the current source first with `cargo install --path . --locked`.

```sh
alias claude='claude-oauth proxy'
```

With an empty store, the alias uses Claude Code's existing authentication. Once you add an account, it uses the selection and monitoring described above. Set `CLAUDE_OAUTH_ACCOUNT=personal` for a single invocation. An explicitly selected missing or paused account produces an error.

```sh
claude                                      # interactive session
CLAUDE_OAUTH_ACCOUNT=personal claude -c      # continue with a specific account
cat notes.txt | claude -p --output-format json "Summarize"
claude mcp list
claude plugin --help
claude --help
```

The installed Claude Code binary interprets every argument, including nested commands, repeated flags, JSON values and literal `--`. For a chat session, the proxy adds only the `--settings` hooks described in [Monitoring](#monitoring-limits-rejected-tokens-and-switching). The terminal, pipes, signals and exit status work as they do with `claude` alone. Other commands are started with Unix `exec`. Use `claude --help` and `claude <command> --help` for your installed version. Anthropic also documents version-specific options in the [CLI reference](https://code.claude.com/docs/en/cli-reference).

Use `claude-oauth` directly to manage saved accounts. For example, `claude rm` operates on a Claude Code session and `claude-oauth rm` removes a saved account. Native `claude auth` commands operate on Claude Code credentials. They do not edit the wrapper's account store. In scripts where shell aliases are unavailable, invoke `claude-oauth proxy` directly.

## Use a saved account as a Paseo provider

On the machine running the Paseo daemon (inside the container for Docker), install Claude Code and a version of `claude-oauth` that lists `proxy` in `claude-oauth --help`. As the daemon's OS user, run `claude-oauth add work`, or use an account already shown by `claude-oauth list`.

Merge the following entry into `agents.providers` in the daemon's `~/.paseo/config.json` (or `$PASEO_HOME/config.json`). Replace both example paths with the absolute paths to the installed binaries on that host. Paseo launches the executable directly, so this also works without a shell alias.

```json
{
  "agents": {
    "providers": {
      "claude-work": {
        "extends": "claude",
        "label": "Claude (Work)",
        "command": ["/absolute/path/to/claude-oauth", "proxy"],
        "env": {
          "CLAUDE_OAUTH_ACCOUNT": "work",
          "CLAUDE_OAUTH_CLAUDE": "/absolute/path/to/claude"
        }
      }
    }
  }
}
```

Run these commands against the same daemon, then choose **Claude (Work)** when starting a new agent:

```sh
paseo reload
paseo provider diagnostic claude-work
```

To follow the wrapper's default account, omit `CLAUDE_OAUTH_ACCOUNT` and select the default with `claude-oauth use <name>`. To offer another account in the picker, copy the entry with a different provider ID, label and account name. Each launch reads the token from the daemon user's account store. If you use a custom store, set `CLAUDE_OAUTH_HOME` in the provider's `env` to that directory. See [Paseo custom providers](https://paseo.sh/docs/custom-providers.md).

## Backups

`backup` encrypts account names, tokens and pause status with a password of at least 12 characters (scrypt N=2¹⁶, r=8, p=1, then AES-256-GCM). The format matches [Relayline](https://github.com/nguyenvanduocit/paseo-relayline)'s account backups, so the two tools can restore each other's backups. For scripts, set `CLAUDE_OAUTH_BACKUP_PASSWORD` instead of typing the password.

## Storage and security

Accounts are stored in `~/.config/claude-oauth/accounts.json` (or `$XDG_CONFIG_HOME/claude-oauth`, or `$CLAUDE_OAUTH_HOME`). What claude-oauth has learned about each account is kept beside it in `state.json`, keyed by a fingerprint of the token rather than the token itself. Deleting `state.json` only forgets limits until the next check or session. The directory is `0700` and the files are `0600`. Updates are atomic and take a lock. Tokens are stored in plain text inside this file, the same way Claude Code keeps credentials on Linux. Only `env` prints a token. Tokens are sent only to `api.anthropic.com`, for the health and quota checks, and to the `claude` process. Claude Code does not pass the token to hooks. The hooks receive the token's fingerprint and look the token up in the store when they need to read usage.

## Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

`.github/workflows/release.yml` checks that the tag matches `Cargo.toml` and runs the tests. It then builds the four binaries, publishes the GitHub Release with `checksums.txt`, and writes `claude-oauth.rb` into [nguyenvanduocit/homebrew-tap](https://github.com/nguyenvanduocit/homebrew-tap). A tag with a suffix such as `v0.2.0-rc.1` becomes a prerelease and does not update the tap. The tap is written with the `HOMEBREW_TAP_DEPLOY_KEY` secret, an SSH deploy key that can push only to the tap.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The setup-token test drives a scripted fake `claude` through a real pseudo-terminal. `tests/monitor.rs` uses a fake `claude` that calls the `--settings` hooks the way Claude Code does, including dropping the token from the hook's environment. It covers switching, stopping, rejected tokens and the direct token check. It also covers Ctrl-C, forwarded SIGTERM, the SIGKILL after the grace period, and restoring a terminal left in raw mode. The setup tests also check that a process started by the fake `claude setup-token` and the temporary profile are gone afterwards. `tests/fixtures/relayline-backup.json` was made by Relayline and keeps the backup format compatible.

## License

MIT
