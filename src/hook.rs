//! `claude-oauth hook <event>`: called by the Claude Code hooks that `run` and `proxy` add
//! to a session. Claude Code passes the hook input on stdin. It removes the OAuth token from a
//! hook's environment, so the session's token fingerprint and launch id travel in variables of
//! our own, and the token is looked up in the account store when it is needed.

use crate::state::{Failure, fingerprint};
use crate::{api, store::Store};
use anyhow::Result;
use std::io::Read;

pub const RUN_ENV: &str = "CLAUDE_OAUTH_RUN";
pub const TOKEN_ENV: &str = "CLAUDE_OAUTH_TOKEN_ID";

pub fn handle(store: &Store, event: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().take(1 << 20).read_to_string(&mut input)?;
    let Some(print) = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|print| !print.is_empty())
    else {
        return Ok(());
    };
    let run = std::env::var(RUN_ENV).ok().filter(|run| !run.is_empty());
    let input: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    let text = |key: &str| {
        input
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    let session_id = text("session_id");
    let now = || chrono::Utc::now().timestamp();
    match event {
        "stop" => store.update_state(|state| {
            state.record_success(&print, run.as_deref(), session_id, now())
        })?,
        "stop-failure" => {
            let kind = text("error").unwrap_or_else(|| "unknown".into());
            let message = text("last_assistant_message").unwrap_or_default();
            // Record at once: Claude Code may exit, and claude-oauth read the state, before a
            // usage reading below comes back.
            store.update_state(|state| {
                state.record_failure(
                    &print,
                    run.as_deref(),
                    session_id.clone(),
                    Failure {
                        kind: &kind,
                        message: &message,
                    },
                    None,
                    now(),
                )
            })?;
            let token = store
                .read()?
                .accounts
                .into_iter()
                .map(|account| account.token)
                .find(|token| fingerprint(token) == print);
            if let (true, Some(token)) = (kind == "rate_limit", token) {
                let (status, quota) = api::quota(&token);
                if status == api::QuotaStatus::Available {
                    store.update_state(|state| {
                        state.record_failure(
                            &print,
                            None,
                            None,
                            Failure {
                                kind: &kind,
                                message: &message,
                            },
                            Some(&quota),
                            now(),
                        )
                    })?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Settings JSON that adds the two hooks to a Claude Code session.
pub fn settings(exe: &std::path::Path) -> String {
    let command = |event: &str| format!("{} hook {event}", crate::quote(&exe.to_string_lossy()));
    serde_json::json!({
        "hooks": {
            "Stop": [{ "hooks": [{ "type": "command", "command": command("stop"), "timeout": 10 }] }],
            "StopFailure": [{ "hooks": [{ "type": "command", "command": command("stop-failure"), "timeout": 30 }] }],
        }
    })
    .to_string()
}
