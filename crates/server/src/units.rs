//! Unit control and status: start/stop/restart and live state, over SSH.

use anyhow::{anyhow, bail};
use nudo_proto::{Service, UnitStatus, unit_action_request};

use crate::ssh::{SshSession, quote};

/// Maps a proto action to its `systemctl` verb.
///
/// `reload` is separate from `restart` on purpose: a service that supports it
/// can pick up new configuration without dropping connections, which matters for
/// exactly the latency-sensitive workloads this tool exists for.
pub fn action_verb(action: unit_action_request::Action) -> anyhow::Result<&'static str> {
    use unit_action_request::Action;
    Ok(match action {
        Action::Start => "start",
        Action::Stop => "stop",
        Action::Restart => "restart",
        Action::Reload => "reload",
        Action::Enable => "enable",
        Action::Disable => "disable",
        Action::Unspecified => bail!("no unit action was specified"),
    })
}

/// Runs a unit action on the target.
pub async fn run_action(
    session: &SshSession,
    unit_name: &str,
    action: unit_action_request::Action,
) -> anyhow::Result<()> {
    let verb = action_verb(action)?;
    session
        .exec(&format!("systemctl {verb} {}", quote(unit_name)))
        .await?
        .require_success(&format!("systemctl {verb}"))?;
    Ok(())
}

/// The `systemctl show` properties we read, as one query.
///
/// One round trip rather than six: the dashboard polls this for every service,
/// and each extra SSH exec is a full channel open.
const SHOW_PROPERTIES: &str = "ActiveState,SubState,UnitFileState,MainPID,\
     MemoryCurrent,NRestarts,ActiveEnterTimestamp";

/// Reads a unit's current state.
///
/// A unit that does not exist yet — a service created but never deployed — is
/// reported as inactive rather than as an error, because that is the accurate
/// answer and the dashboard shows it alongside its siblings.
pub async fn read_status(
    session: &SshSession,
    service: &Service,
    unit_name: &str,
) -> anyhow::Result<UnitStatus> {
    let result = session
        .exec(&format!(
            "systemctl show --property={SHOW_PROPERTIES} {}",
            quote(unit_name)
        ))
        .await?;

    // `systemctl show` succeeds even for an unknown unit, reporting
    // ActiveState=inactive, so a non-zero exit here is a real problem.
    if !result.ok() && result.stdout.trim().is_empty() {
        return Err(anyhow!(
            "reading the state of {unit_name} failed: {}",
            result.stderr.trim()
        ));
    }

    Ok(parse_show_output(&result.stdout, &service.id))
}

/// Parses `systemctl show` key=value output into a status message.
pub fn parse_show_output(output: &str, service_id: &str) -> UnitStatus {
    let mut properties = std::collections::HashMap::new();
    for line in output.lines() {
        if let Some((key, value)) = line.split_once('=') {
            properties.insert(key.trim(), value.trim());
        }
    }

    let get = |key: &str| properties.get(key).copied().unwrap_or_default();

    // systemd reports absent numeric values as the u64 sentinel
    // "[not set]" or as u64::MAX; both mean "no value", not a real number.
    let number = |key: &str| -> u64 {
        let raw = get(key);
        if raw.is_empty() || raw == "[not set]" {
            return 0;
        }
        match raw.parse::<u64>() {
            Ok(value) if value == u64::MAX => 0,
            Ok(value) => value,
            Err(_) => 0,
        }
    };

    UnitStatus {
        service_id: service_id.to_string(),
        active_state: get("ActiveState").to_string(),
        sub_state: get("SubState").to_string(),
        // "enabled-runtime" counts as enabled; "linked" and "masked" do not.
        enabled: matches!(get("UnitFileState"), "enabled" | "enabled-runtime"),
        pid: number("MainPID") as u32,
        memory_bytes: number("MemoryCurrent"),
        since: parse_systemd_timestamp(get("ActiveEnterTimestamp")).map(nudo_proto::to_timestamp),
        restart_count: number("NRestarts") as u32,
    }
}

/// Parses systemd's human-readable timestamp format.
///
/// systemd prints `Wed 2026-07-22 14:05:33 UTC`, which is not RFC 3339, so the
/// date and time are extracted positionally. A blank or unparseable value yields
/// `None` — a missing "since" is not worth failing a status read over.
fn parse_systemd_timestamp(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "n/a" {
        return None;
    }

    // Skip the weekday, then take the date and time.
    let mut parts = raw.split_whitespace();
    let first = parts.next()?;
    // Some locales omit the weekday, so decide by whether the first token looks
    // like a date.
    let (date, time) = if first.contains('-') {
        (first, parts.next()?)
    } else {
        (parts.next()?, parts.next()?)
    };

    chrono::NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

/// A status for a unit that could not be reached, so the dashboard can show the
/// service in the list rather than dropping it.
pub fn unknown_status(service_id: &str) -> UnitStatus {
    UnitStatus {
        service_id: service_id.to_string(),
        active_state: "unknown".to_string(),
        sub_state: String::new(),
        enabled: false,
        pid: 0,
        memory_bytes: 0,
        since: None,
        restart_count: 0,
    }
}

/// Human-readable badge text for a unit state, shared by the dashboard and the
/// CLI so the two never disagree about what "degraded" means.
pub fn status_label(status: &UnitStatus) -> &'static str {
    nudo_format::unit_state_label(status)
}

/// Whether a state should read as healthy in the UI.
pub fn is_healthy(status: &UnitStatus) -> bool {
    status.active_state == "active" && status.sub_state != "dead"
}

#[cfg(test)]
mod tests {
    use super::*;
    use unit_action_request::Action;

    #[test]
    fn every_action_maps_to_its_systemctl_verb() {
        assert_eq!(action_verb(Action::Start).expect("verb"), "start");
        assert_eq!(action_verb(Action::Stop).expect("verb"), "stop");
        assert_eq!(action_verb(Action::Restart).expect("verb"), "restart");
        assert_eq!(action_verb(Action::Reload).expect("verb"), "reload");
        assert_eq!(action_verb(Action::Enable).expect("verb"), "enable");
        assert_eq!(action_verb(Action::Disable).expect("verb"), "disable");
    }

    #[test]
    fn an_unspecified_action_is_refused_rather_than_defaulted() {
        // Defaulting to any verb would let an empty request restart a service.
        assert!(action_verb(Action::Unspecified).is_err());
    }

    #[test]
    fn a_running_units_properties_are_parsed() {
        let output = "ActiveState=active\n\
                      SubState=running\n\
                      UnitFileState=enabled\n\
                      MainPID=4242\n\
                      MemoryCurrent=52428800\n\
                      NRestarts=3\n\
                      ActiveEnterTimestamp=Wed 2026-07-22 14:05:33 UTC\n";

        let status = parse_show_output(output, "svc_1");
        assert_eq!(status.service_id, "svc_1");
        assert_eq!(status.active_state, "active");
        assert_eq!(status.sub_state, "running");
        assert!(status.enabled);
        assert_eq!(status.pid, 4242);
        assert_eq!(status.memory_bytes, 52_428_800);
        assert_eq!(status.restart_count, 3);
        assert!(status.since.is_some());
    }

    #[test]
    fn systemd_sentinels_for_absent_numbers_read_as_zero() {
        // A stopped unit reports MemoryCurrent as the u64 max or "[not set]";
        // showing either as a memory figure would be nonsense.
        let output = "ActiveState=inactive\n\
                      SubState=dead\n\
                      MainPID=0\n\
                      MemoryCurrent=[not set]\n\
                      NRestarts=0\n\
                      ActiveEnterTimestamp=\n";

        let status = parse_show_output(output, "svc_1");
        assert_eq!(status.memory_bytes, 0);
        assert_eq!(status.pid, 0);
        assert!(status.since.is_none());

        let maxed = parse_show_output(&format!("MemoryCurrent={}\n", u64::MAX), "svc_1");
        assert_eq!(maxed.memory_bytes, 0);
    }

    #[test]
    fn only_genuinely_enabled_unit_file_states_count_as_enabled() {
        for (state, expected) in [
            ("enabled", true),
            ("enabled-runtime", true),
            ("disabled", false),
            ("masked", false),
            ("static", false),
            ("linked", false),
            ("", false),
        ] {
            let status = parse_show_output(&format!("UnitFileState={state}\n"), "svc_1");
            assert_eq!(status.enabled, expected, "UnitFileState={state}");
        }
    }

    #[test]
    fn output_with_unexpected_or_missing_lines_still_parses() {
        // Different systemd versions report different property sets.
        let status =
            parse_show_output("ActiveState=failed\nSomethingElse=x\nnot-a-pair\n", "svc_1");
        assert_eq!(status.active_state, "failed");
        assert!(status.sub_state.is_empty());
        assert_eq!(status.pid, 0);
    }

    #[test]
    fn empty_output_yields_an_all_zero_status_rather_than_panicking() {
        let status = parse_show_output("", "svc_1");
        assert_eq!(status.service_id, "svc_1");
        assert!(status.active_state.is_empty());
        assert_eq!(status.pid, 0);
    }

    #[test]
    fn a_value_containing_an_equals_sign_is_not_truncated() {
        let status = parse_show_output("ActiveState=active\nSubState=running=weird\n", "svc_1");
        assert_eq!(status.sub_state, "running=weird");
    }

    #[test]
    fn systemd_timestamps_are_parsed_with_or_without_a_weekday() {
        let with_day = parse_systemd_timestamp("Wed 2026-07-22 14:05:33 UTC").expect("parse");
        assert_eq!(with_day.to_rfc3339(), "2026-07-22T14:05:33+00:00");

        let without_day = parse_systemd_timestamp("2026-07-22 14:05:33 UTC").expect("parse");
        assert_eq!(without_day, with_day);
    }

    #[test]
    fn an_absent_timestamp_is_none_rather_than_an_error() {
        assert!(parse_systemd_timestamp("").is_none());
        assert!(parse_systemd_timestamp("n/a").is_none());
        assert!(parse_systemd_timestamp("garbage").is_none());
        assert!(parse_systemd_timestamp("Wed").is_none());
    }

    #[test]
    fn states_are_labelled_for_display() {
        let label = |active: &str, sub: &str| {
            status_label(&UnitStatus {
                active_state: active.to_string(),
                sub_state: sub.to_string(),
                ..Default::default()
            })
        };

        assert_eq!(label("active", "running"), "running");
        assert_eq!(label("active", "exited"), "exited cleanly");
        assert_eq!(label("activating", "start-pre"), "starting");
        assert_eq!(label("deactivating", "stop"), "stopping");
        assert_eq!(label("failed", "failed"), "failed");
        assert_eq!(label("inactive", "dead"), "stopped");
        assert_eq!(label("unknown", ""), "unreachable");
        assert_eq!(label("something-new", ""), "unknown");
    }

    #[test]
    fn only_an_active_unit_reads_as_healthy() {
        let healthy = |active: &str, sub: &str| {
            is_healthy(&UnitStatus {
                active_state: active.to_string(),
                sub_state: sub.to_string(),
                ..Default::default()
            })
        };

        assert!(healthy("active", "running"));
        assert!(!healthy("failed", "failed"));
        assert!(!healthy("inactive", "dead"));
        assert!(!healthy("activating", "start"));
        assert!(!healthy("unknown", ""));
    }

    #[test]
    fn an_unreachable_target_yields_a_status_that_still_names_the_service() {
        // The dashboard must be able to show the row rather than dropping it.
        let status = unknown_status("svc_9");
        assert_eq!(status.service_id, "svc_9");
        assert_eq!(status_label(&status), "unreachable");
        assert!(!is_healthy(&status));
    }

    #[test]
    fn the_status_query_asks_for_every_property_the_message_reports() {
        for property in [
            "ActiveState",
            "SubState",
            "UnitFileState",
            "MainPID",
            "MemoryCurrent",
            "NRestarts",
            "ActiveEnterTimestamp",
        ] {
            assert!(SHOW_PROPERTIES.contains(property), "missing {property}");
        }
    }
}
