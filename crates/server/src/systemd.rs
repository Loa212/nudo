//! Systemd unit rendering and release path layout.
//!
//! The deploy model is: every release lands in its own directory under
//! `<release_root>/releases/<release_id>/`, and `<release_root>/current` is a
//! symlink to whichever release is live. The unit's `ExecStart` points at the
//! symlink, never at a release directory, so activating a release is a symlink
//! swap plus a restart — and rolling back is the same operation pointed at an
//! older directory.

use std::collections::BTreeMap;

use nudo_proto::Service;

/// Filesystem layout for one service on a target.
///
/// All paths are absolute on the target. Constructed from the service's
/// `release_root`, with a trailing slash trimmed so `/opt/bot/` and `/opt/bot`
/// produce identical paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasePaths {
    root: String,
}

impl ReleasePaths {
    pub fn new(release_root: &str) -> Self {
        let trimmed = release_root.trim().trim_end_matches('/');
        Self {
            root: if trimmed.is_empty() {
                "/".to_string()
            } else {
                trimmed.to_string()
            },
        }
    }

    /// `<root>` — the service's directory on the target.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// `<root>/releases` — parent of every release directory.
    pub fn releases_dir(&self) -> String {
        format!("{}/releases", self.root)
    }

    /// `<root>/releases/<release_id>` — one immutable release.
    pub fn release_dir(&self, release_id: &str) -> String {
        format!("{}/releases/{release_id}", self.root)
    }

    /// `<root>/current` — the symlink the unit's `ExecStart` resolves through.
    pub fn current_link(&self) -> String {
        format!("{}/current", self.root)
    }

    /// The binary inside a specific release.
    pub fn release_binary(&self, release_id: &str, binary_name: &str) -> String {
        format!("{}/{binary_name}", self.release_dir(release_id))
    }

    /// The binary as the unit refers to it: through the `current` symlink, so
    /// the unit file never changes when a new release is activated.
    pub fn current_binary(&self, binary_name: &str) -> String {
        format!("{}/{binary_name}", self.current_link())
    }

    /// The `EnvironmentFile` holding resolved secrets. Kept outside `releases/`
    /// so a retention sweep cannot delete the running service's environment,
    /// and written `0600` owned by the service user.
    pub fn env_file(&self) -> String {
        format!("{}/env", self.root)
    }

    /// Temporary upload destination. A release is staged here and moved into
    /// place only once it has transferred completely, so an interrupted upload
    /// never leaves a half-written binary that `current` could point at.
    pub fn staging_dir(&self, release_id: &str) -> String {
        format!("{}/.staging-{release_id}", self.root)
    }
}

/// The canonical name of the binary inside a release directory. Fixed rather
/// than derived from the artifact's filename so the unit file is stable across
/// deploys whose artifacts happen to be named differently.
pub const BINARY_NAME: &str = "bin";

/// Renders the full systemd unit file for a service.
///
/// This is what `RenderUnit` returns for preview and what the deploy engine
/// writes to the target, from the same code path — a preview that could differ
/// from what gets written would be worse than no preview.
pub fn render_unit(service: &Service) -> String {
    let unit = service.unit.clone().unwrap_or_default();
    let paths = ReleasePaths::new(&service.release_root);
    let mut out = String::new();

    // ---- [Unit] ----
    out.push_str("[Unit]\n");
    let description = if unit.description.trim().is_empty() {
        format!("{} (managed by nudo)", service.name)
    } else {
        unit.description.trim().to_string()
    };
    out.push_str(&format!("Description={description}\n"));

    // network-online is the near-universal want for a service that talks to
    // anything; callers can add more via `after`.
    let mut after: Vec<String> = vec!["network-online.target".to_string()];
    for entry in &unit.after {
        let entry = entry.trim();
        if !entry.is_empty() && !after.iter().any(|a| a == entry) {
            after.push(entry.to_string());
        }
    }
    out.push_str(&format!("After={}\n", after.join(" ")));
    out.push_str("Wants=network-online.target\n");
    out.push('\n');

    // ---- [Service] ----
    out.push_str("[Service]\n");
    out.push_str("Type=simple\n");

    let exec_start = if unit.exec_args.trim().is_empty() {
        paths.current_binary(BINARY_NAME)
    } else {
        format!(
            "{} {}",
            paths.current_binary(BINARY_NAME),
            unit.exec_args.trim()
        )
    };
    out.push_str(&format!("ExecStart={exec_start}\n"));

    let working_directory = if unit.working_directory.trim().is_empty() {
        paths.current_link()
    } else {
        unit.working_directory.trim().to_string()
    };
    out.push_str(&format!("WorkingDirectory={working_directory}\n"));

    if !unit.user.trim().is_empty() {
        out.push_str(&format!("User={}\n", unit.user.trim()));
    }
    if !unit.group.trim().is_empty() {
        out.push_str(&format!("Group={}\n", unit.group.trim()));
    }

    // Non-secret env is inlined; secrets go to the EnvironmentFile so their
    // values never appear in `systemctl cat` output or the unit on disk.
    for (key, value) in sorted(&service.env) {
        out.push_str(&format!(
            "Environment=\"{key}={}\"\n",
            escape_unit_env(value)
        ));
    }
    if !service.secret_ids.is_empty() {
        // The dash makes a missing file non-fatal, which matters on the very
        // first start of a service whose secrets have not been written yet.
        out.push_str(&format!("EnvironmentFile=-{}\n", paths.env_file()));
    }

    let restart = match unit.restart.trim() {
        "" => "always",
        other => other,
    };
    out.push_str(&format!("Restart={restart}\n"));
    let restart_sec = if unit.restart_sec == 0 {
        5
    } else {
        unit.restart_sec
    };
    out.push_str(&format!("RestartSec={restart_sec}\n"));

    // ---- latency knobs ----
    // The reason this tool exists instead of Docker: these are expressible
    // directly on the unit and take effect without a container runtime in the
    // scheduling path.
    if !unit.cpu_affinity.trim().is_empty() {
        out.push_str(&format!("CPUAffinity={}\n", unit.cpu_affinity.trim()));
    }
    if !unit.nice.trim().is_empty() {
        out.push_str(&format!("Nice={}\n", unit.nice.trim()));
    }
    if !unit.io_scheduling_class.trim().is_empty() {
        out.push_str(&format!(
            "IOSchedulingClass={}\n",
            unit.io_scheduling_class.trim()
        ));
    }

    // The escape hatch, written verbatim and last so it can override anything
    // above — an operator who sets `Restart` here means it.
    for (key, value) in sorted(&unit.extra_directives) {
        out.push_str(&format!("{key}={value}\n"));
    }

    out.push('\n');
    out.push_str("[Install]\n");
    out.push_str("WantedBy=multi-user.target\n");

    out
}

/// Normalizes a unit name to end in `.service`, defaulting to the service name
/// when unset. Systemd needs the suffix; users habitually omit it.
pub fn unit_file_name(service: &Service) -> String {
    let raw = service
        .unit
        .as_ref()
        .map(|u| u.unit_name.trim())
        .unwrap_or_default();

    let base = if raw.is_empty() {
        sanitize_unit_stem(&service.name)
    } else {
        raw.to_string()
    };

    if base.ends_with(".service") {
        base
    } else {
        format!("{base}.service")
    }
}

/// Absolute path of the unit file on the target.
pub fn unit_file_path(service: &Service) -> String {
    format!("/etc/systemd/system/{}", unit_file_name(service))
}

/// Reduces an arbitrary service name to something systemd accepts as a unit
/// stem: alphanumerics, dash, underscore and dot.
fn sanitize_unit_stem(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "nudo-service".to_string()
    } else {
        cleaned
    }
}

/// Renders the `EnvironmentFile` contents for a service's resolved secrets.
///
/// Values are quoted and escaped because systemd parses this file, and a secret
/// containing a newline or quote would otherwise truncate the value or introduce
/// a second assignment.
pub fn render_env_file(entries: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in entries {
        out.push_str(&format!("{key}=\"{}\"\n", escape_env_file(value)));
    }
    out
}

/// Escapes a value for a double-quoted assignment in an `EnvironmentFile`.
///
/// Deliberately separate from [`escape_unit_env`]: systemd performs `$VAR`
/// expansion in a unit's `Environment=` directive but **not** in an
/// `EnvironmentFile`, so doubling `$` here would deliver a literal `$$` to the
/// service. Sharing one escaper between the two contexts is what caused exactly
/// that bug, found by the end-to-end test.
fn escape_env_file(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // Kept on one line: a raw newline would end the assignment and let
            // the rest of the value be read as another variable.
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// Escapes a value for a unit file's `Environment=` directive.
///
/// Here `$` *is* expanded by systemd, so a literal one has to be doubled.
fn escape_unit_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '$' => out.push_str("$$"),
            other => out.push(other),
        }
    }
    out
}

/// Deterministic ordering for map-valued fields.
///
/// Protobuf maps have no defined iteration order, so rendering straight from
/// one would produce a different unit file on each call and make every deploy
/// look like a change. Sorting makes the output stable.
fn sorted(map: &std::collections::HashMap<String, String>) -> Vec<(&String, &String)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

/// Chooses which releases to delete, given the ones on disk (newest first) and
/// how many to keep.
///
/// The currently-live release is never a candidate for deletion even if it
/// falls outside the retention window — deleting it would break the running
/// service and leave nothing to roll back to.
pub fn releases_to_prune<'a>(
    releases_newest_first: &[&'a str],
    keep: u32,
    current_release_id: &str,
) -> Vec<&'a str> {
    // Zero would mean "delete everything including what is running"; treat it
    // as the default rather than honoring it.
    let keep = if keep == 0 {
        DEFAULT_KEEP_RELEASES
    } else {
        keep
    } as usize;

    releases_newest_first
        .iter()
        .skip(keep)
        .filter(|id| **id != current_release_id)
        .copied()
        .collect()
}

/// Retained releases when a service does not specify. Enough to roll back past
/// a bad deploy and the one before it.
pub const DEFAULT_KEEP_RELEASES: u32 = 5;

/// Picks the release a rollback should target.
///
/// `requested` empty means "the previous release", which is the newest release
/// that is not the live one. An explicit id is honored only if that release is
/// actually still retained, so a rollback cannot point `current` at a directory
/// that a retention sweep has already removed.
pub fn rollback_target<'a>(
    releases_newest_first: &[&'a str],
    current_release_id: &str,
    requested: &str,
) -> Result<&'a str, RollbackError> {
    if !requested.trim().is_empty() {
        let requested = requested.trim();
        if requested == current_release_id {
            return Err(RollbackError::AlreadyCurrent);
        }
        return releases_newest_first
            .iter()
            .find(|id| **id == requested)
            .copied()
            .ok_or(RollbackError::NotRetained);
    }

    releases_newest_first
        .iter()
        .find(|id| **id != current_release_id)
        .copied()
        .ok_or(RollbackError::NoPreviousRelease)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RollbackError {
    #[error("no previous release to roll back to")]
    NoPreviousRelease,
    #[error("that release is already the current one")]
    AlreadyCurrent,
    #[error("that release is no longer retained on the target")]
    NotRetained,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nudo_proto::SystemdUnit;
    use std::collections::HashMap;

    fn service() -> Service {
        Service {
            id: "svc_1".into(),
            target_id: "tgt_1".into(),
            name: "hft-bot".into(),
            release_root: "/opt/hft-bot".into(),
            unit: Some(SystemdUnit {
                unit_name: "hft-bot.service".into(),
                description: "HFT bot".into(),
                restart: "always".into(),
                restart_sec: 3,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    // ---- paths ----

    #[test]
    fn release_paths_are_built_under_the_release_root() {
        let paths = ReleasePaths::new("/opt/bot");
        assert_eq!(paths.releases_dir(), "/opt/bot/releases");
        assert_eq!(paths.release_dir("r1"), "/opt/bot/releases/r1");
        assert_eq!(paths.current_link(), "/opt/bot/current");
        assert_eq!(paths.current_binary("bin"), "/opt/bot/current/bin");
        assert_eq!(
            paths.release_binary("r1", "bin"),
            "/opt/bot/releases/r1/bin"
        );
        assert_eq!(paths.env_file(), "/opt/bot/env");
    }

    #[test]
    fn a_trailing_slash_on_the_root_does_not_double_up() {
        assert_eq!(
            ReleasePaths::new("/opt/bot/"),
            ReleasePaths::new("/opt/bot")
        );
        assert_eq!(
            ReleasePaths::new("/opt/bot/").current_link(),
            "/opt/bot/current"
        );
    }

    #[test]
    fn the_env_file_sits_outside_the_releases_directory() {
        // Otherwise a retention sweep of old releases could delete the running
        // service's environment.
        let paths = ReleasePaths::new("/opt/bot");
        assert!(!paths.env_file().starts_with(&paths.releases_dir()));
    }

    #[test]
    fn staging_is_outside_releases_so_a_partial_upload_is_never_activatable() {
        let paths = ReleasePaths::new("/opt/bot");
        assert!(!paths.staging_dir("r1").starts_with(&paths.releases_dir()));
    }

    // ---- unit rendering ----

    #[test]
    fn the_unit_execs_through_the_current_symlink_not_a_release_directory() {
        // This is what makes activation a symlink swap instead of a unit rewrite.
        let rendered = render_unit(&service());
        assert!(rendered.contains("ExecStart=/opt/hft-bot/current/bin\n"));
        assert!(!rendered.contains("/releases/"));
    }

    #[test]
    fn exec_args_are_appended_after_the_binary() {
        let mut svc = service();
        svc.unit.as_mut().expect("unit").exec_args = "--config /etc/bot.toml --verbose".into();
        assert!(
            render_unit(&svc)
                .contains("ExecStart=/opt/hft-bot/current/bin --config /etc/bot.toml --verbose\n")
        );
    }

    #[test]
    fn a_rendered_unit_has_the_three_required_sections() {
        let rendered = render_unit(&service());
        assert!(rendered.contains("[Unit]\n"));
        assert!(rendered.contains("[Service]\n"));
        assert!(rendered.contains("[Install]\nWantedBy=multi-user.target\n"));
        assert!(rendered.contains("Description=HFT bot\n"));
    }

    #[test]
    fn the_latency_knobs_are_rendered_when_set() {
        let mut svc = service();
        let unit = svc.unit.as_mut().expect("unit");
        unit.cpu_affinity = "2-5".into();
        unit.nice = "-10".into();
        unit.io_scheduling_class = "realtime".into();

        let rendered = render_unit(&svc);
        assert!(rendered.contains("CPUAffinity=2-5\n"));
        assert!(rendered.contains("Nice=-10\n"));
        assert!(rendered.contains("IOSchedulingClass=realtime\n"));
    }

    #[test]
    fn latency_knobs_are_omitted_entirely_when_unset() {
        // An empty `CPUAffinity=` would pin the service to no CPUs at all.
        let rendered = render_unit(&service());
        assert!(!rendered.contains("CPUAffinity"));
        assert!(!rendered.contains("Nice="));
        assert!(!rendered.contains("IOSchedulingClass"));
    }

    #[test]
    fn extra_directives_are_written_verbatim_and_can_override_earlier_ones() {
        let mut svc = service();
        svc.unit.as_mut().expect("unit").extra_directives = HashMap::from([
            ("LimitNOFILE".to_string(), "1048576".to_string()),
            ("MemoryMax".to_string(), "8G".to_string()),
        ]);

        let rendered = render_unit(&svc);
        assert!(rendered.contains("LimitNOFILE=1048576\n"));
        assert!(rendered.contains("MemoryMax=8G\n"));

        // Later wins in systemd, so the escape hatch must come after the
        // directives it is meant to be able to override.
        let mut svc2 = service();
        svc2.unit.as_mut().expect("unit").extra_directives =
            HashMap::from([("Restart".to_string(), "no".to_string())]);
        let rendered2 = render_unit(&svc2);
        let generated = rendered2.find("Restart=always").expect("generated Restart");
        let override_pos = rendered2.find("Restart=no").expect("override Restart");
        assert!(override_pos > generated);
    }

    #[test]
    fn map_valued_fields_render_in_a_stable_order() {
        // Protobuf map iteration order is unspecified, so rendering twice must
        // still produce identical bytes or every deploy looks like a change.
        let mut svc = service();
        svc.env = HashMap::from([
            ("ZONE".to_string(), "eu".to_string()),
            ("ALPHA".to_string(), "1".to_string()),
            ("MIDDLE".to_string(), "2".to_string()),
        ]);
        svc.unit.as_mut().expect("unit").extra_directives = HashMap::from([
            ("ZZZ".to_string(), "z".to_string()),
            ("AAA".to_string(), "a".to_string()),
        ]);

        let first = render_unit(&svc);
        for _ in 0..8 {
            assert_eq!(render_unit(&svc), first);
        }

        let alpha = first.find("ALPHA").expect("ALPHA");
        let middle = first.find("MIDDLE").expect("MIDDLE");
        let zone = first.find("ZONE").expect("ZONE");
        assert!(alpha < middle && middle < zone);
    }

    #[test]
    fn non_secret_env_is_inlined_but_secrets_go_to_an_environment_file() {
        let mut svc = service();
        svc.env = HashMap::from([("LOG_LEVEL".to_string(), "info".to_string())]);
        svc.secret_ids = vec!["sec_1".into()];

        let rendered = render_unit(&svc);
        assert!(rendered.contains("Environment=\"LOG_LEVEL=info\"\n"));
        // `-` prefix: the file legitimately does not exist before the first
        // deploy writes it.
        assert!(rendered.contains("EnvironmentFile=-/opt/hft-bot/env\n"));
        // Secret ids must never appear in the unit itself.
        assert!(!rendered.contains("sec_1"));
    }

    #[test]
    fn no_environment_file_is_referenced_when_a_service_has_no_secrets() {
        assert!(!render_unit(&service()).contains("EnvironmentFile"));
    }

    #[test]
    fn env_values_needing_escapes_cannot_break_out_of_the_directive() {
        let mut svc = service();
        svc.env = HashMap::from([(
            "TRICKY".to_string(),
            "a\"b\\c\nRestart=no\n$HOME".to_string(),
        )]);

        let rendered = render_unit(&svc);
        let line = rendered
            .lines()
            .find(|l| l.starts_with("Environment=\"TRICKY="))
            .expect("TRICKY line");

        // The injected directive must stay inside the quoted value.
        assert!(line.contains("\\n"));
        assert!(!rendered.contains("\nRestart=no\n"));
        assert!(line.contains("\\\""));
        assert!(
            line.contains("$$HOME"),
            "literal $ must be escaped for systemd"
        );
    }

    #[test]
    fn network_online_is_ordered_before_the_service_and_extra_after_entries_are_kept() {
        let mut svc = service();
        svc.unit.as_mut().expect("unit").after =
            vec!["postgresql.service".into(), "network-online.target".into()];

        let rendered = render_unit(&svc);
        let after = rendered
            .lines()
            .find(|l| l.starts_with("After="))
            .expect("After line");
        assert!(after.contains("network-online.target"));
        assert!(after.contains("postgresql.service"));
        // Deduplicated even though the caller repeated it.
        assert_eq!(after.matches("network-online.target").count(), 1);
    }

    #[test]
    fn restart_defaults_are_applied_when_unset() {
        let mut svc = service();
        let unit = svc.unit.as_mut().expect("unit");
        unit.restart = String::new();
        unit.restart_sec = 0;

        let rendered = render_unit(&svc);
        assert!(rendered.contains("Restart=always\n"));
        assert!(rendered.contains("RestartSec=5\n"));
    }

    #[test]
    fn user_and_group_are_omitted_when_empty_so_the_service_runs_as_root() {
        let rendered = render_unit(&service());
        assert!(!rendered.contains("User="));
        assert!(!rendered.contains("Group="));

        let mut svc = service();
        let unit = svc.unit.as_mut().expect("unit");
        unit.user = "bot".into();
        unit.group = "bot".into();
        let rendered = render_unit(&svc);
        assert!(rendered.contains("User=bot\n"));
        assert!(rendered.contains("Group=bot\n"));
    }

    #[test]
    fn working_directory_defaults_to_the_current_symlink() {
        assert!(render_unit(&service()).contains("WorkingDirectory=/opt/hft-bot/current\n"));

        let mut svc = service();
        svc.unit.as_mut().expect("unit").working_directory = "/var/lib/bot".into();
        assert!(render_unit(&svc).contains("WorkingDirectory=/var/lib/bot\n"));
    }

    #[test]
    fn a_missing_description_falls_back_to_the_service_name() {
        let mut svc = service();
        svc.unit.as_mut().expect("unit").description = String::new();
        assert!(render_unit(&svc).contains("Description=hft-bot (managed by nudo)\n"));
    }

    // ---- unit naming ----

    #[test]
    fn unit_names_always_end_in_dot_service() {
        let mut svc = service();
        svc.unit.as_mut().expect("unit").unit_name = "hft-bot".into();
        assert_eq!(unit_file_name(&svc), "hft-bot.service");

        svc.unit.as_mut().expect("unit").unit_name = "hft-bot.service".into();
        assert_eq!(unit_file_name(&svc), "hft-bot.service");
    }

    #[test]
    fn an_unset_unit_name_is_derived_from_the_service_name() {
        let mut svc = service();
        svc.unit.as_mut().expect("unit").unit_name = String::new();
        svc.name = "My Bot!".into();
        assert_eq!(unit_file_name(&svc), "My-Bot.service");
    }

    #[test]
    fn a_service_name_with_no_usable_characters_still_yields_a_valid_unit() {
        let mut svc = service();
        svc.unit = Some(SystemdUnit::default());
        svc.name = "///".into();
        assert_eq!(unit_file_name(&svc), "nudo-service.service");
    }

    #[test]
    fn unit_files_are_written_under_etc_systemd_system() {
        assert_eq!(
            unit_file_path(&service()),
            "/etc/systemd/system/hft-bot.service"
        );
    }

    #[test]
    fn a_service_with_no_unit_message_still_renders() {
        let mut svc = service();
        svc.unit = None;
        let rendered = render_unit(&svc);
        assert!(rendered.contains("ExecStart=/opt/hft-bot/current/bin\n"));
        assert_eq!(unit_file_name(&svc), "hft-bot.service");
    }

    // ---- environment file ----

    #[test]
    fn the_env_file_quotes_values_and_escapes_newlines() {
        let entries = BTreeMap::from([
            ("API_KEY".to_string(), "abc123".to_string()),
            ("PEM".to_string(), "line1\nline2".to_string()),
        ]);
        let rendered = render_env_file(&entries);
        assert_eq!(rendered, "API_KEY=\"abc123\"\nPEM=\"line1\\nline2\"\n");
        // One key per line, so a multi-line secret cannot introduce a second
        // assignment.
        assert_eq!(rendered.lines().count(), 2);
    }

    #[test]
    fn a_dollar_sign_reaches_the_service_literally_through_an_environment_file() {
        // systemd expands `$VAR` in a unit's `Environment=` directive but not in
        // an `EnvironmentFile`. Doubling the `$` in both places delivered a
        // literal `$$` to the service — a real bug the end-to-end test caught.
        let entries = BTreeMap::from([("APP_TOKEN".to_string(), "p@ss$word".to_string())]);
        let rendered = render_env_file(&entries);
        assert_eq!(rendered, "APP_TOKEN=\"p@ss$word\"\n");
        assert!(
            !rendered.contains("$$"),
            "an EnvironmentFile must not double $"
        );
    }

    #[test]
    fn a_dollar_sign_is_doubled_in_a_unit_directive_where_systemd_expands_it() {
        // The other half of the same distinction: here it must be escaped, or
        // systemd substitutes an empty value.
        let mut svc = service();
        svc.env =
            std::collections::HashMap::from([("PROMPT".to_string(), "$HOME/bin".to_string())]);
        let rendered = render_unit(&svc);
        assert!(
            rendered.contains("Environment=\"PROMPT=$$HOME/bin\""),
            "got: {rendered}"
        );
    }

    // ---- retention ----

    #[test]
    fn retention_keeps_the_newest_n_releases() {
        let releases = ["r5", "r4", "r3", "r2", "r1"];
        assert_eq!(releases_to_prune(&releases, 3, "r5"), vec!["r2", "r1"]);
    }

    #[test]
    fn retention_never_prunes_the_live_release() {
        // r1 is outside the window but is what is running.
        let releases = ["r5", "r4", "r3", "r2", "r1"];
        let pruned = releases_to_prune(&releases, 2, "r1");
        assert!(!pruned.contains(&"r1"));
        assert_eq!(pruned, vec!["r3", "r2"]);
    }

    #[test]
    fn nothing_is_pruned_when_fewer_releases_exist_than_the_keep_count() {
        assert!(releases_to_prune(&["r2", "r1"], 5, "r2").is_empty());
        assert!(releases_to_prune(&[], 5, "").is_empty());
    }

    #[test]
    fn a_keep_count_of_zero_falls_back_to_the_default() {
        // Honoring zero literally would delete the running release.
        let releases = ["r2", "r1"];
        assert!(releases_to_prune(&releases, 0, "r2").is_empty());
    }

    // ---- rollback selection ----

    #[test]
    fn an_empty_request_rolls_back_to_the_release_before_the_current_one() {
        let releases = ["r3", "r2", "r1"];
        assert_eq!(rollback_target(&releases, "r3", "").expect("target"), "r2");
    }

    #[test]
    fn the_previous_release_skips_the_current_one_even_if_it_is_not_newest() {
        // After a rollback, `current` is an older release than the newest on
        // disk; "previous" must still mean "not what is running".
        let releases = ["r3", "r2", "r1"];
        assert_eq!(rollback_target(&releases, "r2", "").expect("target"), "r3");
    }

    #[test]
    fn an_explicit_release_is_honored_when_still_retained() {
        let releases = ["r3", "r2", "r1"];
        assert_eq!(
            rollback_target(&releases, "r3", "r1").expect("target"),
            "r1"
        );
        assert_eq!(
            rollback_target(&releases, "r3", " r1 ").expect("target"),
            "r1"
        );
    }

    #[test]
    fn rolling_back_to_a_pruned_release_is_refused() {
        // Pointing `current` at a directory that no longer exists would break
        // the service on the next restart.
        assert_eq!(
            rollback_target(&["r3", "r2"], "r3", "r1"),
            Err(RollbackError::NotRetained)
        );
    }

    #[test]
    fn rolling_back_to_the_current_release_is_refused() {
        assert_eq!(
            rollback_target(&["r2", "r1"], "r2", "r2"),
            Err(RollbackError::AlreadyCurrent)
        );
    }

    #[test]
    fn rollback_fails_when_there_is_only_one_release() {
        assert_eq!(
            rollback_target(&["r1"], "r1", ""),
            Err(RollbackError::NoPreviousRelease)
        );
        assert_eq!(
            rollback_target(&[], "", ""),
            Err(RollbackError::NoPreviousRelease)
        );
    }
}
