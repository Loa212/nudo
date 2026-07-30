//! How a tool result is worded for an agent.
//!
//! Everything an agent reads that is *also* read by the dashboard or the CLI —
//! unit states, artifact sources, log levels — comes from `nudo_format`, so the
//! three surfaces cannot describe the same thing differently. What is left here
//! is the one thing only this crate needs.

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
