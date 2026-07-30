use super::*;

pub(super) async fn sources(cli: &Cli) -> anyhow::Result<()> {
    let mut client = cli.client()?.sources();
    let response = client.list(ListSourcesRequest {}).await?.into_inner();

    let sources = response.sources;
    emit(cli, &JsonSources::from(&sources), || {
        let rows: Vec<Vec<String>> = sources
            .iter()
            .map(|source| {
                vec![
                    source.id.clone(),
                    source.name.clone(),
                    source::Kind::try_from(source.kind)
                        .unwrap_or(source::Kind::Unspecified)
                        .as_str()
                        .to_string(),
                    if source.account_login.is_empty() {
                        "-".to_string()
                    } else {
                        source.account_login.clone()
                    },
                    if source.installed {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ]
            })
            .collect();
        format::table(&["id", "name", "kind", "account", "installed"], &rows)
    });

    Ok(())
}
