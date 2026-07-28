use super::*;

#[test]
fn a_digest_is_only_ever_shown_as_a_prefix() {
    assert_eq!(digest_prefix("0123456789abcdef0123"), "0123456789ab");
    assert_eq!(digest_prefix(""), "-");
}

#[test]
fn a_short_sha_is_eight_characters_and_a_missing_one_is_a_dash() {
    assert_eq!(short_sha("0123456789abcdef"), "01234567");
    assert_eq!(short_sha(""), "-");
}

#[test]
fn a_missing_string_field_renders_as_a_dash() {
    assert_eq!(or_dash(""), "-");
    assert_eq!(or_dash("   "), "-");
    assert_eq!(or_dash("value"), "value");
}

#[test]
fn latency_knobs_are_detected_from_any_one_of_them() {
    let with = |unit: SystemdUnit| {
        has_latency_knobs(&Service {
            unit: Some(unit),
            ..Default::default()
        })
    };

    assert!(with(SystemdUnit {
        cpu_affinity: "0-3".to_string(),
        ..Default::default()
    }));
    assert!(with(SystemdUnit {
        nice: "-5".to_string(),
        ..Default::default()
    }));
    assert!(with(SystemdUnit {
        io_scheduling_class: "realtime".to_string(),
        ..Default::default()
    }));
    assert!(!with(SystemdUnit::default()));
    assert!(!has_latency_knobs(&Service::default()));
}

#[test]
fn wide_tables_are_wrapped_so_the_page_does_not_scroll_sideways() {
    let screens = [
        s(targets_list(&[a_target()])),
        s(services_list(
            &[a_service()],
            &[a_target()],
            &HashMap::new(),
        )),
        s(deployments_list(
            &[Deployment {
                id: "d".to_string(),
                ..Default::default()
            }],
            &[a_service()],
        )),
        s(audit_list(&[AuditEntry {
            id: "a".to_string(),
            ..Default::default()
        }])),
        s(secrets_list(
            &[Secret {
                id: "s".to_string(),
                ..Default::default()
            }],
            &[],
            &[],
            SecretNotice::None,
            "t",
        )),
    ];
    for rendered in screens {
        assert!(rendered.contains("class=\"table-scroll\""));
    }
}

// -----------------------------------------------------------------------
// Updates, the changelog and the support banner
// -----------------------------------------------------------------------
