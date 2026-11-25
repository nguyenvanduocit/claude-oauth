//! Last-known account state, kept in `state.json` beside the account store.
//!
//! Entries are keyed by a fingerprint of the token, so renaming an account keeps its state and
//! replacing a token starts fresh. The state is a cache: losing it only costs a new check.

use crate::api::{Health, QuotaStatus, Report, Window, window_label};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn fingerprint(token: &str) -> String {
    Sha256::digest(token.as_bytes())[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub tokens: BTreeMap<String, TokenState>,
    /// What the Claude Code hooks reported for each `run`/`proxy` launch.
    #[serde(default)]
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_status: Option<QuotaStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota: Vec<Window>,
    /// The account should not be picked before this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resting_until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub run: String,
    pub at: i64,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `ok` for a finished turn, else Claude Code's StopFailure error type.
    pub kind: String,
}

/// A StopFailure from Claude Code: its error type and the message shown to the user.
pub struct Failure<'a> {
    pub kind: &'a str,
    pub message: &'a str,
}

/// How an account looks right now, for display and for picking one.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Unchecked,
    Ready,
    Resting { until: i64, reason: String },
    Rejected,
    Forbidden(Option<String>),
    RateLimited,
    Offline,
}

impl Status {
    /// Whether `run` may pick the account on its own.
    pub fn usable(&self) -> bool {
        matches!(
            self,
            Status::Unchecked | Status::Ready | Status::RateLimited | Status::Offline
        )
    }

    pub fn describe(&self) -> String {
        match self {
            Status::Unchecked => "not checked yet".into(),
            Status::Ready => "ready".into(),
            Status::Resting { until, reason } => format!("{reason}, resets {}", clock(*until)),
            Status::Rejected => "token rejected (expired or revoked)".into(),
            Status::Forbidden(Some(detail)) => format!("blocked: {detail}"),
            Status::Forbidden(None) => "blocked by Anthropic".into(),
            Status::RateLimited => "throttled by Anthropic".into(),
            Status::Offline => "could not reach Anthropic".into(),
        }
    }

    pub fn short(&self) -> &'static str {
        match self {
            Status::Unchecked => "unchecked",
            Status::Ready => "ready",
            Status::Resting { .. } => "limit",
            Status::Rejected => "rejected",
            Status::Forbidden(_) => "blocked",
            Status::RateLimited => "throttled",
            Status::Offline => "offline",
        }
    }
}

/// Local wall-clock time, with the weekday when it is not today.
pub fn clock(unix: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(time) = Local.timestamp_opt(unix, 0).single() else {
        return "later".into();
    };
    if time.date_naive() == Local::now().date_naive() {
        time.format("%H:%M").to_string()
    } else {
        time.format("%a %H:%M").to_string()
    }
}

/// When every exhausted window lifts, and which one is the reason.
fn exhausted(quota: &[Window], now: i64) -> Option<(i64, String)> {
    quota
        .iter()
        .filter(|window| window.used_percent >= 100.0)
        .filter_map(|window| Some((window.resets_at.filter(|at| *at > now)?, window.id.as_str())))
        .max_by_key(|(at, _)| *at)
        .map(|(at, id)| (at, format!("{} limit reached", window_label(id))))
}

/// How long to rest an account after a rate limit whose reset time is unknown.
fn fallback_rest(message: &str) -> (i64, String) {
    let text = message.to_lowercase();
    if text.contains("weekly") || text.contains("monthly") {
        (6 * 3600, "weekly limit reached".into())
    } else if text.contains("limit") {
        (3600, "usage limit reached".into())
    } else {
        (5 * 60, "throttled by Anthropic".into())
    }
}

impl State {
    pub fn token(&self, fingerprint: &str) -> Option<&TokenState> {
        self.tokens.get(fingerprint)
    }

    pub fn status(&self, token: &str, now: i64) -> Status {
        let Some(entry) = self.tokens.get(&fingerprint(token)) else {
            return Status::Unchecked;
        };
        match entry.health {
            Some(Health::Rejected) => return Status::Rejected,
            Some(Health::Forbidden) => return Status::Forbidden(entry.detail.clone()),
            _ => {}
        }
        if let Some(until) = entry.resting_until.filter(|until| *until > now) {
            let reason = entry
                .rest_reason
                .clone()
                .unwrap_or_else(|| "usage limit reached".into());
            return Status::Resting { until, reason };
        }
        match entry.health {
            Some(Health::Ready) => Status::Ready,
            Some(Health::RateLimited) => Status::RateLimited,
            Some(Health::Offline) => Status::Offline,
            _ => Status::Unchecked,
        }
    }

    /// Store a fresh `check`. A readable quota decides whether the account rests.
    pub fn record_check(&mut self, token: &str, report: &Report, now: i64) {
        let entry = self.tokens.entry(fingerprint(token)).or_default();
        entry.checked_at = Some(now);
        entry.health = Some(report.health);
        entry.detail = report.detail.clone();
        if report.health != Health::Ready {
            return;
        }
        entry.quota_status = Some(report.quota_status);
        if report.quota_status == QuotaStatus::Available {
            entry.quota = report.quota.clone();
            match exhausted(&report.quota, now) {
                Some((until, reason)) => {
                    entry.resting_until = Some(until);
                    entry.rest_reason = Some(reason);
                }
                None => {
                    entry.resting_until = None;
                    entry.rest_reason = None;
                }
            }
        }
    }

    /// A turn finished normally, so the account works right now.
    /// Keyed by token fingerprint: Claude Code does not pass the token itself to hooks.
    pub fn record_success(
        &mut self,
        print: &str,
        run: Option<&str>,
        session_id: Option<String>,
        now: i64,
    ) {
        let print = print.to_string();
        let entry = self.tokens.entry(print.clone()).or_default();
        entry.health = Some(Health::Ready);
        entry.detail = None;
        entry.resting_until = None;
        entry.rest_reason = None;
        if let Some(run) = run {
            self.events.push(Event {
                run: run.into(),
                at: now,
                fingerprint: print,
                session_id,
                kind: "ok".into(),
            });
        }
    }

    /// A turn ended with Claude Code's StopFailure. `quota` is a usage reading taken after a
    /// rate limit, when one could be made.
    pub fn record_failure(
        &mut self,
        print: &str,
        run: Option<&str>,
        session_id: Option<String>,
        failure: Failure,
        quota: Option<&[Window]>,
        now: i64,
    ) {
        let Failure { kind, message } = failure;
        let print = print.to_string();
        let entry = self.tokens.entry(print.clone()).or_default();
        match kind {
            "rate_limit" => {
                let (until, reason) = quota
                    .and_then(|quota| exhausted(quota, now))
                    .unwrap_or_else(|| {
                        let (seconds, reason) = fallback_rest(message);
                        (now + seconds, reason)
                    });
                if let Some(quota) = quota.filter(|quota| !quota.is_empty()) {
                    entry.quota = quota.to_vec();
                    entry.quota_status = Some(QuotaStatus::Available);
                }
                entry.resting_until = Some(until);
                entry.rest_reason = Some(reason);
            }
            "authentication_failed" => {
                entry.health = Some(Health::Rejected);
                entry.checked_at = Some(now);
            }
            "oauth_org_not_allowed"
            | "account_on_hold"
            | "verification_required"
            | "billing_error" => {
                entry.health = Some(Health::Forbidden);
                entry.detail = Some(kind.replace('_', " "));
                entry.checked_at = Some(now);
            }
            _ => {}
        }
        if let Some(run) = run {
            self.events.push(Event {
                run: run.into(),
                at: now,
                fingerprint: print,
                session_id,
                kind: kind.into(),
            });
        }
    }

    pub fn last_event(&self, run: &str) -> Option<&Event> {
        self.events.iter().rev().find(|event| event.run == run)
    }

    pub fn prune(&mut self, now: i64) {
        self.events.retain(|event| now - event.at < 7 * 86_400);
        let excess = self.events.len().saturating_sub(200);
        self.events.drain(..excess);
    }
}

/// StopFailure kinds after which another account may succeed.
pub fn account_failure(kind: &str) -> bool {
    matches!(
        kind,
        "rate_limit"
            | "authentication_failed"
            | "oauth_org_not_allowed"
            | "account_on_hold"
            | "verification_required"
            | "billing_error"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;
    const TOKEN: &str = "sk-ant-oat01-state";

    fn window(id: &str, used: f64, resets_at: Option<i64>) -> Window {
        Window {
            id: id.into(),
            used_percent: used,
            resets_at,
        }
    }

    fn report(quota: Vec<Window>) -> Report {
        Report {
            health: Health::Ready,
            detail: None,
            quota_status: QuotaStatus::Available,
            quota,
        }
    }

    #[test]
    fn an_exhausted_window_rests_the_account_until_it_resets() {
        let mut state = State::default();
        state.record_check(
            TOKEN,
            &report(vec![
                window("five_hour", 100.0, Some(NOW + 600)),
                window("seven_day", 40.0, None),
            ]),
            NOW,
        );
        let status = state.status(TOKEN, NOW);
        assert_eq!(
            status,
            Status::Resting {
                until: NOW + 600,
                reason: "5-hour limit reached".into()
            }
        );
        assert!(!status.usable());
        assert_eq!(state.status(TOKEN, NOW + 601), Status::Ready);
        state.record_check(
            TOKEN,
            &report(vec![window("five_hour", 10.0, Some(NOW + 9000))]),
            NOW + 5,
        );
        assert_eq!(state.status(TOKEN, NOW + 5), Status::Ready);
    }

    #[test]
    fn an_unreadable_quota_keeps_a_known_rest() {
        let mut state = State::default();
        state.record_failure(
            &fingerprint(TOKEN),
            Some("r1"),
            None,
            Failure {
                kind: "rate_limit",
                message: "You've hit your 5-hour limit",
            },
            None,
            NOW,
        );
        let unavailable = Report {
            health: Health::Ready,
            detail: None,
            quota_status: QuotaStatus::RateLimited,
            quota: vec![],
        };
        state.record_check(TOKEN, &unavailable, NOW + 10);
        assert!(
            matches!(state.status(TOKEN, NOW + 10), Status::Resting { until, .. } if until == NOW + 3600)
        );
    }

    #[test]
    fn hook_failures_map_to_statuses_and_success_clears_a_rest() {
        let mut state = State::default();
        state.record_failure(
            &fingerprint(TOKEN),
            Some("r1"),
            Some("s1".into()),
            Failure {
                kind: "rate_limit",
                message: "",
            },
            Some(&[window("seven_day", 100.0, Some(NOW + 7200))]),
            NOW,
        );
        assert_eq!(
            state.status(TOKEN, NOW),
            Status::Resting {
                until: NOW + 7200,
                reason: "weekly limit reached".into()
            }
        );
        assert_eq!(
            state.last_event("r1").unwrap().session_id.as_deref(),
            Some("s1")
        );
        state.record_success(&fingerprint(TOKEN), Some("r2"), None, NOW + 1);
        assert_eq!(state.status(TOKEN, NOW + 1), Status::Ready);

        state.record_failure(
            &fingerprint(TOKEN),
            None,
            None,
            Failure {
                kind: "billing_error",
                message: "",
            },
            None,
            NOW,
        );
        assert_eq!(
            state.status(TOKEN, NOW),
            Status::Forbidden(Some("billing error".into()))
        );
        state.record_failure(
            &fingerprint(TOKEN),
            None,
            None,
            Failure {
                kind: "authentication_failed",
                message: "",
            },
            None,
            NOW,
        );
        assert_eq!(state.status(TOKEN, NOW), Status::Rejected);
        state.record_failure(
            &fingerprint("sk-ant-oat01-other"),
            None,
            None,
            Failure {
                kind: "overloaded",
                message: "",
            },
            None,
            NOW,
        );
        assert_eq!(state.status("sk-ant-oat01-other", NOW), Status::Unchecked);
    }

    #[test]
    fn prunes_old_events() {
        let mut state = State::default();
        state.record_failure(
            &fingerprint(TOKEN),
            Some("old"),
            None,
            Failure {
                kind: "rate_limit",
                message: "",
            },
            None,
            NOW - 8 * 86_400,
        );
        state.record_failure(
            &fingerprint(TOKEN),
            Some("new"),
            None,
            Failure {
                kind: "rate_limit",
                message: "",
            },
            None,
            NOW,
        );
        state.prune(NOW);
        assert_eq!(state.events.len(), 1);
        assert_eq!(fingerprint(TOKEN).len(), 24);
    }
}
