//! Log streaming from a target's journal.
//!
//! `journalctl --output=json` is read over SSH and parsed into structured lines.
//! JSON rather than the human format because it carries the cursor, the real
//! timestamp and the priority as separate fields — a cursor is what lets a
//! dropped connection resume instead of re-reading from the start.

use anyhow::Context;
use tokio::sync::mpsc;

use crate::events::{Bus, LogEvent};
use crate::ssh::{SshSession, quote};

/// One journald entry, as `journalctl --output=json` emits it.
///
/// Only the fields we use are named; journald includes dozens more.
#[derive(Debug, serde::Deserialize)]
struct JournalEntry {
    #[serde(rename = "MESSAGE", default)]
    message: MessageField,
    #[serde(rename = "PRIORITY", default)]
    priority: Option<String>,
    #[serde(rename = "_SYSTEMD_UNIT", default)]
    unit: Option<String>,
    #[serde(rename = "__CURSOR", default)]
    cursor: Option<String>,
    /// Microseconds since the epoch, as a string.
    #[serde(rename = "__REALTIME_TIMESTAMP", default)]
    realtime: Option<String>,
}

/// A journald `MESSAGE` is usually a string, but is an array of byte values when
/// the line is not valid UTF-8.
#[derive(Debug, Default)]
struct MessageField(String);

impl<'de> serde::Deserialize<'de> for MessageField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(MessageField(match value {
            serde_json::Value::String(text) => text,
            serde_json::Value::Array(bytes) => {
                let raw: Vec<u8> = bytes
                    .iter()
                    .filter_map(|b| b.as_u64())
                    .map(|b| b as u8)
                    .collect();
                String::from_utf8_lossy(&raw).into_owned()
            }
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        }))
    }
}

/// How a log read should be scoped.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub follow: bool,
    /// How many past lines to include before following.
    pub tail_lines: u32,
    /// Resume after this journald cursor.
    pub since_cursor: String,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// Only lines containing this text. Applied by journald, not by us, so a
    /// filtered follow does not ship every line over the SSH channel.
    pub grep: String,
}

/// Builds the `journalctl` command for a query.
pub fn build_command(unit_name: &str, query: &LogQuery) -> String {
    let mut command = format!("journalctl --unit={} --output=json --no-pager", quote(unit_name));

    // A cursor is more precise than a timestamp, so it wins when both are given.
    if !query.since_cursor.trim().is_empty() {
        command.push_str(&format!(
            " --after-cursor={}",
            quote(query.since_cursor.trim())
        ));
    } else if let Some(since) = query.since {
        command.push_str(&format!(
            " --since={}",
            quote(&since.format("%Y-%m-%d %H:%M:%S").to_string())
        ));
    } else if query.tail_lines > 0 {
        // Capped: a client asking for a million lines would hold the channel
        // open for minutes and pin memory on both ends.
        command.push_str(&format!(" --lines={}", query.tail_lines.min(50_000)));
    } else {
        command.push_str(" --lines=100");
    }

    if !query.grep.trim().is_empty() {
        // journald does the filtering, so a quiet match in a noisy service does
        // not transfer everything.
        command.push_str(&format!(" --grep={}", quote(query.grep.trim())));
    }

    if query.follow {
        command.push_str(" --follow");
    }

    command
}

/// Reads logs, sending each parsed line to `sink`.
///
/// Returns when the command exits, which for a `--follow` read means when the
/// receiver hangs up or the connection drops.
pub async fn stream(
    session: &SshSession,
    unit_name: &str,
    query: &LogQuery,
    sink: mpsc::Sender<LogEvent>,
) -> anyhow::Result<()> {
    let command = build_command(unit_name, query);
    let (raw_tx, mut raw_rx) = mpsc::channel(256);

    let forward = tokio::spawn(async move {
        while let Some(line) = raw_rx.recv().await {
            let line: crate::ssh::OutputLine = line;
            // journalctl writes diagnostics to stderr; those are not entries.
            if line.stderr {
                continue;
            }
            if let Some(event) = parse_line(&line.text) {
                if sink.send(event).await.is_err() {
                    return;
                }
            }
        }
    });

    let result = session.exec_streaming(&command, raw_tx).await;
    let _ = forward.await;

    result.context("reading the journal")?;
    Ok(())
}

/// Parses one line of `journalctl --output=json` output.
///
/// Returns `None` for anything unparseable rather than failing the stream: a
/// single malformed entry should not end a log view.
pub fn parse_line(line: &str) -> Option<LogEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let entry: JournalEntry = serde_json::from_str(line).ok()?;

    let at = entry
        .realtime
        .as_deref()
        .and_then(|micros| micros.parse::<i64>().ok())
        .and_then(chrono::DateTime::from_timestamp_micros)
        .unwrap_or_else(chrono::Utc::now);

    Some(LogEvent {
        at,
        message: entry.message.0,
        priority: entry.priority.unwrap_or_default(),
        unit: entry.unit.unwrap_or_default(),
        cursor: entry.cursor.unwrap_or_default(),
    })
}

/// Keeps a service's ring buffer warm by tailing its journal in the background.
///
/// This is what makes a freshly opened log view non-empty. The tail stops when
/// nothing is watching, so an idle control plane holds no SSH connections to a
/// latency-critical box.
pub async fn tail_into_buffer(
    session: &SshSession,
    bus: &Bus,
    service_id: &str,
    unit_name: &str,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel(256);
    let bus = bus.clone();
    let service_id = service_id.to_string();

    let collect = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            bus.publish_log(&service_id, event);
        }
    });

    let query = LogQuery {
        follow: true,
        tail_lines: 200,
        ..Default::default()
    };
    let result = stream(session, unit_name, &query, tx).await;
    let _ = collect.await;
    result
}

/// Maps a journald numeric priority to a level name for display.
pub fn priority_label(priority: &str) -> &'static str {
    match priority.trim() {
        "0" => "emerg",
        "1" => "alert",
        "2" => "crit",
        "3" => "err",
        "4" => "warning",
        "5" => "notice",
        "6" => "info",
        "7" => "debug",
        _ => "info",
    }
}

/// Whether a priority is at or above warning, for the log viewer's level filter.
pub fn is_problem(priority: &str) -> bool {
    matches!(priority.trim(), "0" | "1" | "2" | "3" | "4")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_reads_structured_output_for_one_unit() {
        let command = build_command("bot.service", &LogQuery::default());
        // JSON so the cursor, timestamp and priority arrive as fields.
        assert!(command.contains("--output=json"));
        assert!(command.contains("--unit=bot.service"));
        assert!(command.contains("--no-pager"));
        // A default read is bounded.
        assert!(command.contains("--lines=100"));
        assert!(!command.contains("--follow"));
    }

    #[test]
    fn a_follow_query_adds_follow() {
        let command = build_command(
            "bot.service",
            &LogQuery {
                follow: true,
                ..Default::default()
            },
        );
        assert!(command.contains("--follow"));
    }

    #[test]
    fn a_cursor_takes_precedence_over_a_timestamp_and_a_line_count() {
        // Resuming from a cursor is exact; re-applying --lines would duplicate.
        let command = build_command(
            "bot.service",
            &LogQuery {
                since_cursor: "s=abc;i=1;b=xyz".to_string(),
                since: Some(chrono::Utc::now()),
                tail_lines: 500,
                ..Default::default()
            },
        );
        assert!(command.contains("--after-cursor="));
        assert!(!command.contains("--since="));
        assert!(!command.contains("--lines="));
    }

    #[test]
    fn a_since_timestamp_is_formatted_the_way_journalctl_expects() {
        let since = chrono::DateTime::parse_from_rfc3339("2026-07-22T14:05:33Z")
            .expect("parse")
            .with_timezone(&chrono::Utc);
        let command = build_command(
            "bot.service",
            &LogQuery {
                since: Some(since),
                ..Default::default()
            },
        );
        assert!(command.contains("--since='2026-07-22 14:05:33'"), "got: {command}");
    }

    #[test]
    fn a_huge_tail_request_is_capped() {
        let command = build_command(
            "bot.service",
            &LogQuery {
                tail_lines: 10_000_000,
                ..Default::default()
            },
        );
        assert!(command.contains("--lines=50000"));
    }

    #[test]
    fn grep_is_pushed_down_to_journald() {
        // Filtering server-side means a quiet match in a noisy service does not
        // transfer every line over the SSH channel.
        let command = build_command(
            "bot.service",
            &LogQuery {
                grep: "ERROR".to_string(),
                ..Default::default()
            },
        );
        assert!(command.contains("--grep=ERROR"));
    }

    #[test]
    fn a_hostile_unit_name_or_grep_pattern_cannot_run_a_second_command() {
        let command = build_command(
            "bot.service; rm -rf /",
            &LogQuery {
                grep: "$(id)".to_string(),
                since_cursor: "'; whoami; '".to_string(),
                ..Default::default()
            },
        );
        assert!(!command.contains("; rm -rf / "), "unquoted unit name");
        assert!(command.contains("'bot.service; rm -rf /'"));
        assert!(command.contains("'$(id)'"));
    }

    #[test]
    fn a_journal_entry_is_parsed_into_its_fields() {
        let line = r#"{"__CURSOR":"s=abc;i=2f;b=xyz","__REALTIME_TIMESTAMP":"1784900733000000","PRIORITY":"6","_SYSTEMD_UNIT":"bot.service","MESSAGE":"listening on :9000"}"#;

        let event = parse_line(line).expect("parse");
        assert_eq!(event.message, "listening on :9000");
        assert_eq!(event.priority, "6");
        assert_eq!(event.unit, "bot.service");
        assert_eq!(event.cursor, "s=abc;i=2f;b=xyz");
        // The timestamp comes from the journal, not from now().
        assert_eq!(event.at.timestamp(), 1_784_900_733);
    }

    #[test]
    fn a_non_utf8_message_arriving_as_a_byte_array_is_recovered() {
        // journald encodes MESSAGE as an array when the bytes are not valid
        // UTF-8; treating that as a parse failure would drop the line.
        let line = r#"{"MESSAGE":[104,105,255,33],"PRIORITY":"6"}"#;
        let event = parse_line(line).expect("parse");
        assert!(event.message.starts_with("hi"));
        assert!(event.message.ends_with('!'));
    }

    #[test]
    fn a_malformed_line_is_skipped_rather_than_ending_the_stream() {
        assert!(parse_line("not json at all").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        // Valid JSON that is not an object.
        assert!(parse_line("[1,2,3]").is_none());
    }

    #[test]
    fn an_entry_missing_optional_fields_still_parses() {
        let event = parse_line(r#"{"MESSAGE":"bare"}"#).expect("parse");
        assert_eq!(event.message, "bare");
        assert!(event.priority.is_empty());
        assert!(event.cursor.is_empty());
        // No timestamp in the entry, so it is stamped on arrival rather than
        // being dropped.
        assert!(event.at <= chrono::Utc::now());
    }

    #[test]
    fn an_entry_with_a_null_message_parses_as_empty() {
        let event = parse_line(r#"{"MESSAGE":null,"PRIORITY":"3"}"#).expect("parse");
        assert!(event.message.is_empty());
        assert_eq!(event.priority, "3");
    }

    #[test]
    fn an_unparseable_timestamp_falls_back_to_now() {
        let event = parse_line(r#"{"MESSAGE":"x","__REALTIME_TIMESTAMP":"not-a-number"}"#)
            .expect("parse");
        assert!(event.at <= chrono::Utc::now());
    }

    #[test]
    fn journald_priorities_are_labelled() {
        assert_eq!(priority_label("0"), "emerg");
        assert_eq!(priority_label("3"), "err");
        assert_eq!(priority_label("4"), "warning");
        assert_eq!(priority_label("6"), "info");
        assert_eq!(priority_label("7"), "debug");
        // An unexpected or missing priority reads as info rather than crashing.
        assert_eq!(priority_label(""), "info");
        assert_eq!(priority_label("99"), "info");
    }

    #[test]
    fn warnings_and_worse_are_flagged_as_problems() {
        assert!(is_problem("0"));
        assert!(is_problem("3"));
        assert!(is_problem("4"));
        assert!(!is_problem("5"));
        assert!(!is_problem("6"));
        assert!(!is_problem("7"));
        assert!(!is_problem(""));
    }
}
