//! Health-check evaluation.
//!
//! A deploy is not finished when the process starts; it is finished when the
//! service is actually serving. The three check kinds are all evaluated over the
//! same SSH connection the deploy used, so a check runs from the target's own
//! network position rather than from the control plane — a bind-to-localhost
//! service is checkable, and a firewall between the control plane and the target
//! cannot produce a false failure.

use std::time::Duration;

use nudo_proto::{HealthCheck, health_check};

use crate::ssh::{SshSession, quote};

/// The outcome of evaluating a health check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthOutcome {
    /// The service answered.
    Healthy,
    /// Every attempt failed; carries the last reason.
    Unhealthy { attempts: u32, detail: String },
}

impl HealthOutcome {
    pub fn healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// A short reason for the deployment's `error` field.
    pub fn detail(&self) -> String {
        match self {
            Self::Healthy => String::new(),
            Self::Unhealthy { attempts, detail } => {
                format!("health check failed after {attempts} attempt(s): {detail}")
            }
        }
    }
}

/// The effective parameters of a check, with unset values defaulted.
///
/// A zero timeout or retry count from the wire means "not set", not "give up
/// immediately", so those are normalized once here rather than at each use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthParams {
    pub timeout: Duration,
    /// Total attempts, always at least one.
    pub attempts: u32,
    pub initial_delay: Duration,
}

impl HealthParams {
    pub fn from_proto(check: Option<&HealthCheck>) -> Self {
        let timeout_seconds = check.map(|c| c.timeout_seconds).filter(|t| *t > 0).unwrap_or(10);
        // `retries` counts retries after the first try, so total attempts is
        // one more; an unset value gives three attempts.
        let retries = check.map(|c| c.retries).unwrap_or(2);
        let initial_delay = check.map(|c| c.initial_delay_seconds).unwrap_or(2);

        Self {
            // Cap so a bad value cannot wedge a deploy indefinitely.
            timeout: Duration::from_secs(timeout_seconds.min(600) as u64),
            attempts: retries.saturating_add(1).min(100),
            initial_delay: Duration::from_secs(initial_delay.min(600) as u64),
        }
    }
}

/// Builds the shell command that evaluates a check on the target.
///
/// Returns `None` for a check kind that needs no command beyond the unit query,
/// which the caller handles directly.
pub fn check_command(check: Option<&HealthCheck>, unit_name: &str) -> String {
    match check.and_then(|c| c.kind.as_ref()) {
        Some(health_check::Kind::HttpUrl(url)) if !url.trim().is_empty() => {
            // `--fail` makes a 4xx/5xx a non-zero exit, which is the whole
            // point: a service returning 500 is not healthy. curl is on every
            // realistic target; wget is the fallback for minimal images.
            let url = quote(url.trim());
            format!(
                "curl --fail --silent --show-error --max-time 10 --output /dev/null {url} \
                 || wget --quiet --spider --timeout=10 {url}"
            )
        }
        Some(health_check::Kind::Command(command)) if !command.trim().is_empty() => {
            // Run through `sh -c` so the operator can use a pipeline or a
            // redirect, which is what most real checks look like.
            format!("sh -c {}", quote(command.trim()))
        }
        // `systemd_active`, and the degenerate cases where a kind was selected
        // but left blank.
        _ => format!("systemctl is-active --quiet {}", quote(unit_name)),
    }
}

/// Evaluates a health check over an existing SSH session.
///
/// Sleeps `initial_delay` first — a service that needs a moment to bind a port
/// would otherwise fail its first attempt for no good reason — then retries up
/// to `attempts` times.
pub async fn evaluate(
    session: &SshSession,
    check: Option<&HealthCheck>,
    unit_name: &str,
    mut on_attempt: impl FnMut(u32, u32, &str),
) -> HealthOutcome {
    let params = HealthParams::from_proto(check);
    let command = check_command(check, unit_name);

    if !params.initial_delay.is_zero() {
        tokio::time::sleep(params.initial_delay).await;
    }

    let mut last_detail = String::from("no attempt was made");

    for attempt in 1..=params.attempts {
        match session.exec_timeout(&command, params.timeout).await {
            Ok(result) if result.ok() => {
                on_attempt(attempt, params.attempts, "ok");
                return HealthOutcome::Healthy;
            }
            Ok(result) => {
                let stderr = result.stderr.trim();
                let stdout = result.stdout.trim();
                last_detail = if !stderr.is_empty() {
                    format!("exit {}: {stderr}", result.exit_code)
                } else if !stdout.is_empty() {
                    format!("exit {}: {stdout}", result.exit_code)
                } else {
                    format!("exit {}", result.exit_code)
                };
            }
            Err(error) => last_detail = error.to_string(),
        }

        on_attempt(attempt, params.attempts, &last_detail);

        // No sleep after the final attempt — it would just delay the failure.
        if attempt < params.attempts {
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }

    HealthOutcome::Unhealthy {
        attempts: params.attempts,
        detail: last_detail,
    }
}

/// Wait between failed attempts.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;

    fn http(url: &str) -> HealthCheck {
        HealthCheck {
            kind: Some(health_check::Kind::HttpUrl(url.to_string())),
            ..Default::default()
        }
    }

    #[test]
    fn an_http_check_fails_the_command_on_a_non_2xx_response() {
        // Without --fail, curl exits 0 on a 500 and a broken service would
        // deploy as healthy.
        let command = check_command(Some(&http("http://127.0.0.1:8080/healthz")), "bot.service");
        assert!(command.contains("--fail"));
        assert!(command.contains("http://127.0.0.1:8080/healthz"));
        // A fallback for images without curl.
        assert!(command.contains("wget"));
    }

    #[test]
    fn a_hostile_health_check_url_cannot_run_a_second_command() {
        let command = check_command(
            Some(&http("http://x/; rm -rf /tmp/evidence")),
            "bot.service",
        );
        assert!(!command.contains("; rm -rf /tmp/evidence "), "unquoted injection");
        assert!(command.contains("'http://x/; rm -rf /tmp/evidence'"));
    }

    #[test]
    fn a_command_check_runs_through_a_shell_so_pipelines_work() {
        let check = HealthCheck {
            kind: Some(health_check::Kind::Command(
                "test $(curl -s localhost/ready) = ok".to_string(),
            )),
            ..Default::default()
        };
        let command = check_command(Some(&check), "bot.service");
        assert!(command.starts_with("sh -c "));
        // The whole pipeline is one quoted argument to sh.
        assert!(command.contains("curl -s localhost/ready"));
    }

    #[test]
    fn a_systemd_check_queries_the_unit_quietly() {
        let check = HealthCheck {
            kind: Some(health_check::Kind::SystemdActive(true)),
            ..Default::default()
        };
        assert_eq!(
            check_command(Some(&check), "bot.service"),
            "systemctl is-active --quiet bot.service"
        );
    }

    #[test]
    fn an_absent_or_blank_check_falls_back_to_the_unit_state() {
        // Never "no check at all": a deploy that does not verify anything is
        // the failure mode this tool exists to avoid.
        assert_eq!(
            check_command(None, "bot.service"),
            "systemctl is-active --quiet bot.service"
        );
        assert_eq!(
            check_command(Some(&http("   ")), "bot.service"),
            "systemctl is-active --quiet bot.service"
        );
        assert_eq!(
            check_command(Some(&HealthCheck::default()), "bot.service"),
            "systemctl is-active --quiet bot.service"
        );
    }

    #[test]
    fn a_unit_name_needing_quoting_is_quoted() {
        let command = check_command(None, "weird name.service");
        assert!(command.contains("'weird name.service'"));
    }

    #[test]
    fn retries_are_converted_to_a_total_attempt_count() {
        // `retries` is retries-after-the-first, so 2 retries is 3 attempts.
        let params = HealthParams::from_proto(Some(&HealthCheck {
            retries: 2,
            timeout_seconds: 5,
            initial_delay_seconds: 1,
            ..Default::default()
        }));
        assert_eq!(params.attempts, 3);
        assert_eq!(params.timeout, Duration::from_secs(5));
        assert_eq!(params.initial_delay, Duration::from_secs(1));
    }

    #[test]
    fn zero_retries_still_means_one_attempt() {
        // Zero attempts would pass a deploy without checking anything.
        let params = HealthParams::from_proto(Some(&HealthCheck {
            retries: 0,
            ..Default::default()
        }));
        assert_eq!(params.attempts, 1);
    }

    #[test]
    fn unset_parameters_get_usable_defaults() {
        let params = HealthParams::from_proto(None);
        assert_eq!(params.attempts, 3);
        assert_eq!(params.timeout, Duration::from_secs(10));
        assert_eq!(params.initial_delay, Duration::from_secs(2));

        // A zero timeout on the wire means unset, not "time out instantly".
        let zeroed = HealthParams::from_proto(Some(&HealthCheck {
            timeout_seconds: 0,
            ..Default::default()
        }));
        assert_eq!(zeroed.timeout, Duration::from_secs(10));
    }

    #[test]
    fn absurd_parameters_are_capped_so_a_deploy_cannot_hang_forever() {
        let params = HealthParams::from_proto(Some(&HealthCheck {
            timeout_seconds: u32::MAX,
            retries: u32::MAX,
            initial_delay_seconds: u32::MAX,
            ..Default::default()
        }));
        assert_eq!(params.timeout, Duration::from_secs(600));
        assert_eq!(params.attempts, 100);
        assert_eq!(params.initial_delay, Duration::from_secs(600));
    }

    #[test]
    fn a_healthy_outcome_carries_no_error_detail() {
        assert!(HealthOutcome::Healthy.healthy());
        assert!(HealthOutcome::Healthy.detail().is_empty());
    }

    #[test]
    fn an_unhealthy_outcome_explains_how_many_attempts_were_made_and_why_it_failed() {
        let outcome = HealthOutcome::Unhealthy {
            attempts: 3,
            detail: "exit 7: connection refused".to_string(),
        };
        assert!(!outcome.healthy());

        let detail = outcome.detail();
        assert!(detail.contains("3 attempt"));
        assert!(detail.contains("connection refused"));
    }
}
