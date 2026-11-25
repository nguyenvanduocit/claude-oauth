//! Token health and quota readings from api.anthropic.com.

use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Ready,
    Rejected,
    Unreachable,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Window {
    pub id: &'static str,
    pub label: &'static str,
    pub used_percent: f64,
    /// RFC 3339 time at which the window resets.
    pub resets_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    Available,
    /// Anthropic throttles the usage endpoint; try again in a few minutes.
    RateLimited,
    /// The account's plan does not expose usage.
    Restricted,
    Unavailable,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub health: Health,
    pub quota_status: QuotaStatus,
    pub quota: Vec<Window>,
}

impl Report {
    pub fn window(&self, id: &str) -> Option<&Window> {
        self.quota.iter().find(|window| window.id == id)
    }

    /// The tighter of the 5-hour and 7-day windows, used to rank accounts for `run --auto`.
    pub fn pressure(&self) -> Option<f64> {
        ["five_hour", "seven_day"]
            .iter()
            .filter_map(|id| self.window(id).map(|window| window.used_percent))
            .reduce(f64::max)
    }
}

const WINDOWS: [(&str, &str); 4] = [
    ("five_hour", "5 hours"),
    ("seven_day", "7 days"),
    ("seven_day_sonnet", "Sonnet · 7 days"),
    ("seven_day_opus", "Opus · 7 days"),
];

pub fn parse_quota(body: &serde_json::Value) -> Vec<Window> {
    WINDOWS
        .iter()
        .filter_map(|(id, label)| {
            let window = body.get(id)?;
            let used = window
                .get("utilization")?
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)?;
            let resets_at = window
                .get("resets_at")
                .and_then(|value| value.as_str())
                .filter(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
                .map(str::to_string);
            Some(Window {
                id,
                label,
                used_percent: used.min(100.0),
                resets_at,
            })
        })
        .collect()
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(8))
        .redirects(0)
        .user_agent(concat!("claude-oauth/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn get(agent: &ureq::Agent, url: &str, token: &str) -> Result<ureq::Response, Box<ureq::Error>> {
    agent
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(Box::new)
}

pub fn check(token: &str) -> Report {
    let agent = agent();
    let health = match get(&agent, "https://api.anthropic.com/v1/models?limit=1", token) {
        Ok(_) => Health::Ready,
        Err(error) if matches!(*error, ureq::Error::Status(401, _)) => Health::Rejected,
        Err(_) => Health::Unreachable,
    };
    if health != Health::Ready {
        return Report {
            health,
            quota_status: QuotaStatus::Unavailable,
            quota: Vec::new(),
        };
    }
    let (quota_status, quota) =
        match get(&agent, "https://api.anthropic.com/api/oauth/usage", token) {
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
        };
    Report {
        health,
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
            .map(|handle| {
                handle.join().unwrap_or(Report {
                    health: Health::Unreachable,
                    quota_status: QuotaStatus::Unavailable,
                    quota: Vec::new(),
                })
            })
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
        assert_eq!(
            windows[0].resets_at.as_deref(),
            Some("2026-11-25T18:00:00+00:00")
        );
        assert_eq!(windows[1].used_percent, 100.0);
        let report = Report {
            health: Health::Ready,
            quota_status: QuotaStatus::Available,
            quota: windows,
        };
        assert_eq!(report.pressure(), Some(100.0));
    }
}
