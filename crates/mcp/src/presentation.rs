use nudo_proto::{Service, UnitStatus};

/// A one-line description of where a service's binary comes from.
pub fn describe_artifact(service: &Service) -> String {
    nudo_format::artifact_description(service)
}

/// A word for a unit's state, so an agent does not have to interpret
/// systemd's two-field vocabulary.
pub fn describe_unit_state(status: &UnitStatus) -> &'static str {
    nudo_format::unit_state_label(status)
}

/// A level name for a journald numeric priority.
pub fn describe_priority(priority: &str) -> &'static str {
    nudo_format::journal_priority_label(priority)
}

/// Renders a command and its arguments for display, quoting anything that
/// contains whitespace or a shell metacharacter.
///
/// For the agent's benefit only — the server quotes independently before the
/// command reaches a target.
pub fn describe_command(command: &str, args: &[String]) -> String {
    let quote = |value: &str| -> String {
        if value.is_empty()
            || value
                .chars()
                .any(|c| c.is_whitespace() || "';|&$`\"\\<>()*?[]{}!#~".contains(c))
        {
            format!("'{}'", value.replace('\'', r"'\''"))
        } else {
            value.to_string()
        }
    };

    let mut rendered = quote(command);
    for arg in args {
        rendered.push(' ');
        rendered.push_str(&quote(arg));
    }
    rendered
}
