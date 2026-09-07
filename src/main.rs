mod app;
mod cli;
mod config;
mod favs;
mod providers;

use std::io::IsTerminal;

use clap::Parser;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Search {
            query,
            source,
            max,
            json,
        } => cmd_search(query, source, max, json).await,
        Command::Favs { json } => cmd_favs(json).await,
        Command::Tui { query, favs } => cmd_tui(query, favs).await,
    }
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build http client: {e}"))
}

async fn cmd_search(
    query: String,
    source: String,
    max: usize,
    json: bool,
) -> anyhow::Result<()> {
    let cfg = config::config();
    let client = http_client()?;
    let source = providers::Source::parse(&source)?;
    let limit = max.clamp(1, 100);
    let results = providers::search(&client, cfg, source, &query, limit).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for r in &results {
            println!("{}", r.url);
        }
    }
    Ok(())
}

async fn cmd_favs(json: bool) -> anyhow::Result<()> {
    let cfg = config::config();
    let client = favs::FavsClient::new(cfg)?;
    let items = client.list().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        for item in &items {
            println!("{}", item.url);
        }
    }
    Ok(())
}

async fn cmd_tui(query: Option<String>, favs_only: bool) -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() && !std::io::stdout().is_terminal() {
        anyhow::bail!("stdin/stdout are not terminals; the TUI requires an interactive session");
    }
    let cfg = config::config();
    let client = http_client()?;

    let items = if favs_only {
        let favs = favs::FavsClient::new(cfg)?;
        let listed = favs.list().await?;
        listed
            .into_iter()
            .map(|f| {
                let title = if f.title.is_empty() {
                    f.id.clone()
                } else {
                    f.title
                };
                app::UrlItem {
                    title,
                    url: f.url,
                }
            })
            .collect::<Vec<_>>()
    } else if let Some(q) = query {
        let results = providers::search(&client, cfg, providers::Source::Auto, &q, 50).await?;
        results
            .into_iter()
            .map(|r| app::UrlItem {
                title: r.title,
                url: r.url,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let heading = if favs_only { "Favorites" } else { "Search" };
    if let Some(url) = app::run(items, heading)? {
        println!("{url}");
        app::copy_to_clipboard(&url);
    }
    Ok(())
}