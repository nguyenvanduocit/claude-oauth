//! Token health and quota readings from api.anthropic.com.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// What the token check said about a token.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// The token works.
    Ready,
    /// 401: the token is expired, revoked or wrong.
    Rejected,
    /// 403: the token is valid but the account or organisation may not use it.
    Forbidden,
    /// 429 on the check itself: the token is valid but Anthropic is throttling it.
    RateLimited,
    /// No answer (network, DNS, 5xx).
    Offline,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    Available,
    /// Anthropic throttles the usage endpoint; try again in a few minutes.
    RateLimited,
    /// The account's plan does not expose usage.
    Restricted,
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub id: String,
    pub used_percent: f64,
    /// Unix time at which the window resets.
    pub resets_at: Option<i64>,
}

pub fn window_label(id: &str) -> &'static str {
    match id {
        "five_hour" => "5-hour",
        "seven_day" => "weekly",
        "seven_day_sonnet" => "weekly Sonnet",
        "seven_day_opus" => "weekly Opus",
        _ => "usage",
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub health: Health,
    /// Anthropic's explanation for a 403.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub quota_status: QuotaStatus,
    pub quota: Vec<Window>,
}

impl Report {
    pub fn offline() -> Self {
        Self {
            health: Health::Offline,
            detail: None,
            quota_status: QuotaStatus::Unavailable,
            quota: Vec::new(),
        }
    }
}

const WINDOWS: [&str; 4] = [
    "five_hour",
    "seven_day",
    "seven_day_sonnet",
    "seven_day_opus",
];

pub fn parse_quota(body: &serde_json::Value) -> Vec<Window> {
    WINDOWS
        .iter()
        .filter_map(|id| {
            let window = body.get(id)?;
            let used = window
                .get("utilization")?
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)?;
            let resets_at = window
                .get("resets_at")
                .and_then(|value| value.as_str())
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|time| time.timestamp());
            Some(Window {
                id: id.to_string(),
                used_percent: used.min(100.0),
                resets_at,
            })
        })
        .collect()
}

/// `CLAUDE_OAUTH_API_URL` exists for tests; normal use talks to api.anthropic.com.
fn base() -> String {
    std::env::var("CLAUDE_OAUTH_API_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://api.anthropic.com".into())
}

fn agent() -> ureq::Agent {
    agent_with(Duration::from_secs(8))
}

fn agent_with(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(0)
        .user_agent(concat!("claude-oauth/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn get(agent: &ureq::Agent, path: &str, token: &str) -> Result<ureq::Response, Box<ureq::Error>> {
    agent
        .get(&format!("{}{path}", base()))
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(Box::new)
}

/// Anthropic's `{"error": {"message": …}}`, shortened for one line.
fn explanation(response: ureq::Response) -> Option<String> {
    let body: serde_json::Value = response.into_json().ok()?;
    let message = body.pointer("/error/message")?.as_str()?.trim();
    let mut short: String = message.chars().take(160).collect();
    if short.len() < message.len() {
        short.push('…');
    }
    (!short.is_empty()).then_some(short)
}

/// Read the usage windows only; used after a rate-limit failure to learn when it lifts.
pub fn quota(token: &str) -> (QuotaStatus, Vec<Window>) {
    match get(&agent(), "/api/oauth/usage", token) {
        Ok(response) => {
            let quota = response
                .into_json::<serde_json::Value>()
                .map(|body| parse_quota(&body))
                .unwrap_or_default();
            let status = if quota.is_empty() {
                QuotaStatus::Unavailable
            } else {
                QuotaStatus::Available
            };
            (status, quota)
        }
        Err(error) => match *error {
            ureq::Error::Status(429, _) => (QuotaStatus::RateLimited, Vec::new()),
            ureq::Error::Status(403, _) => (QuotaStatus::Restricted, Vec::new()),
            _ => (QuotaStatus::Unavailable, Vec::new()),
        },
    }
}

/// Whether the token works, without the quota, waiting at most `timeout`.
pub fn health(token: &str, timeout: Duration) -> Report {
    let (health, detail) = token_health(&agent_with(timeout), token);
    Report {
        health,
        detail,
        quota_status: QuotaStatus::Unavailable,
        quota: Vec::new(),
    }
}

fn token_health(agent: &ureq::Agent, token: &str) -> (Health, Option<String>) {
    match get(agent, "/v1/models?limit=1", token) {
        Ok(_) => (Health::Ready, None),
        Err(error) => match *error {
            ureq::Error::Status(401, _) => (Health::Rejected, None),
            ureq::Error::Status(403, response) => (Health::Forbidden, explanation(response)),
            ureq::Error::Status(429, _) => (Health::RateLimited, None),
            _ => (Health::Offline, None),
        },
    }
}

pub fn check(token: &str) -> Report {
    let (health, detail) = token_health(&agent(), token);
    if health != Health::Ready {
        return Report {
            health,
            detail,
            quota_status: QuotaStatus::Unavailable,
            quota: Vec::new(),
        };
    }
    let (quota_status, quota) = quota(token);
    Report {
        health,
        detail,
        quota_status,
        quota,
    }
}

/// Check several tokens at once, keeping their order.
pub fn check_all(tokens: &[String]) -> Vec<Report> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = tokens
            .iter()
            .map(|token| scope.spawn(move || check(token)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|_| Report::offline()))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_known_windows_and_ignores_the_rest() {
        let body = serde_json::json!({
            "five_hour": { "utilization": 42.5, "resets_at": "2026-11-25T18:00:00+00:00" },
            "seven_day": { "utilization": 130, "resets_at": null },
            "seven_day_opus": { "utilization": -1 },
            "extra_usage": { "utilization": 5 },
        });
        let windows = parse_quota(&body);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used_percent, 42.5);
        assert_eq!(windows[0].resets_at, Some(1_795_629_600));
        assert_eq!(windows[1].used_percent, 100.0);
        assert_eq!(windows[1].resets_at, None);
    }
}
