use super::*;

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// The audit log.
///
/// The point of this screen is telling a human clicking deploy apart from an
/// agent calling an MCP tool, so the actor kind is a column rather than a
/// tooltip, and a refusal is coloured like a failure — a refused mutation is the
/// guardrail working, and it is what someone reading this log is looking for.
pub fn audit_list(entries: &[AuditEntry]) -> Markup {
    html! {
        (topbar("Audit", Some("Every mutating call, and who made it"), html! {}))
        div .content {
            div .card.pad-0 {
                @if entries.is_empty() {
                    div .card-body { p .muted { "Nothing recorded yet." } }
                } @else {
                    div .table-scroll {
                        table {
                            thead {
                                tr {
                                    th { "When" }
                                    th { "Kind" }
                                    th { "Actor" }
                                    th { "Action" }
                                    th { "Subject" }
                                    th { "Mode" }
                                    th { "Summary" }
                                }
                            }
                            tbody {
                                @for entry in entries {
                                    @let refused = entry.action.contains("refused");
                                    tr {
                                        td .nowrap.small.muted { (ago(entry.at.as_ref())) }
                                        td {
                                            @match entry.actor.as_ref().map(|a| a.kind_str()) {
                                                Some("human") => (badge("human", BadgeKind::Neutral)),
                                                // An agent acting on production is
                                                // the case worth spotting quickly.
                                                Some("agent") => (badge("agent", BadgeKind::Info)),
                                                Some("webhook") => (badge("webhook", BadgeKind::Info)),
                                                Some("system") => (badge("system", BadgeKind::Neutral)),
                                                _ => (badge("unknown", BadgeKind::Neutral)),
                                            }
                                        }
                                        td .small {
                                            (entry.actor.as_ref().map(|a| or_dash(&a.label)).unwrap_or_else(|| "-".to_string()))
                                        }
                                        td .small {
                                            @if refused {
                                                span .badge.bad { (entry.action) }
                                            } @else {
                                                span .mono { (entry.action) }
                                            }
                                        }
                                        td .mono.small { (or_dash(&entry.subject_id)) }
                                        td {
                                            @if entry.dry_run {
                                                // A dry run changed nothing, so it
                                                // must not read like a real change.
                                                (badge("dry run", BadgeKind::Warn))
                                            } @else {
                                                span .faint.small { "applied" }
                                            }
                                        }
                                        td .small { (truncate(&entry.summary, 80)) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
