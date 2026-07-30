use super::*;

pub(super) async fn audit(cli: &Cli, subject: Option<&str>, limit: u32) -> anyhow::Result<()> {
    let mut client = cli.client()?.audit();

    let response = client
        .list(ListAuditRequest {
            subject_id: subject.unwrap_or_default().to_string(),
            actor_kind: actor::Kind::Unspecified as i32,
            page_size: limit,
            page_token: String::new(),
        })
        .await?
        .into_inner();

    let entries = response.entries;
    emit(cli, &JsonAudit::from(&entries), || {
        let rows: Vec<Vec<String>> = entries
            .iter()
            .map(|entry| {
                vec![
                    format::ago(entry.at.as_ref()),
                    entry
                        .actor
                        .as_ref()
                        .map(|a| a.kind_str().to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    entry
                        .actor
                        .as_ref()
                        .map(|a| a.label.clone())
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(|| "-".to_string()),
                    entry.action.clone(),
                    if entry.dry_run {
                        "yes".to_string()
                    } else {
                        "-".to_string()
                    },
                    format::truncate(&entry.summary, 60),
                ]
            })
            .collect();
        format::table(
            &["when", "kind", "actor", "action", "dry run", "summary"],
            &rows,
        )
    });

    Ok(())
}
