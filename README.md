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
| `list` (`ls`) `[--json]` | List accounts; `*` marks the default. Works offline. |
| `check` (`status`) `[name] [--json]` | Token health plus quota left in the 5-hour and 7-day windows |
| `use <name>` | Set the default account |
| `run [-a name \| --auto] [-q] [-- claude args…]` | Replace this process with `claude`, using the account's token |
| `env [name]` | Print shell exports: `eval "$(claude-oauth env work)"` |
| `rename <name> <new>` | Rename an account |
| `pause <name>` / `resume <name>` | `run` skips paused accounts |
| `remove` (`rm`) `<name> [-y]` | Forget an account. The token itself stays valid until you revoke it at claude.ai. |
| `backup [-o file\|-] [--force]` | Write a password-encrypted backup |
| `restore <file\|->` | Restore from a backup and skip tokens that are already saved |

`run` chooses the account in this order: `-a`, then `$CLAUDE_OAUTH_ACCOUNT`, then the default account, then the first one that is not paused. `--auto` checks every account that is not paused and picks the working one whose tighter quota window has the most room. Accounts whose quota cannot be read are tried after those with quota left. Claude Code starts with `CLAUDE_CODE_OAUTH_TOKEN` set and `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` cleared, so the chosen account is the one being billed. Everything else, including your Claude Code settings, history and MCP servers, stays the same.

An alias makes it transparent:

```sh
alias claude='claude-oauth run -q --'
```

## Backups

`backup` encrypts account names, tokens and pause status with a password of at least 12 characters (scrypt N=2¹⁶, r=8, p=1, then AES-256-GCM). The format matches [Relayline](https://github.com/nguyenvanduocit/paseo-relayline)'s account backups, so the two tools can restore each other's backups. For scripts, set `CLAUDE_OAUTH_BACKUP_PASSWORD` instead of typing the password.

## Storage and security

Accounts are stored in `~/.config/claude-oauth/accounts.json` (or `$XDG_CONFIG_HOME/claude-oauth`, or `$CLAUDE_OAUTH_HOME`). The directory is `0700` and the file is `0600`. Updates are atomic and lock the file. Tokens are stored in plain text inside this file, the same way Claude Code keeps credentials on Linux. Only `env` prints a token. Tokens are sent only to `api.anthropic.com`, for the health and quota checks, and to the `claude` process.

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

The setup-token test drives a scripted fake `claude` through a real pseudo-terminal. `tests/fixtures/relayline-backup.json` was made by Relayline and keeps the backup format compatible.

## License

MIT
