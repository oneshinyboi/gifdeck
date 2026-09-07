mod app;
mod cli;
mod clipboard;
mod config;
mod favs;
mod preview;
mod providers;

use std::collections::HashSet;
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

async fn cmd_search(query: String, source: String, max: usize, json: bool) -> anyhow::Result<()> {
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

/// Build the unified picker App: favorites client + favorite-ID set
/// (best-effort — the picker still opens when the server is unreachable,
/// surfacing the error in the footer), initial search / favorites pages
/// when the CLI asked for them.
async fn cmd_tui(query: Option<String>, favs_only: bool) -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() && !std::io::stdout().is_terminal() {
        anyhow::bail!("stdin/stdout are not terminals; the TUI requires an interactive session");
    }
    let cfg = config::config();
    let http = http_client()?;

    let mut picker = app::App::new(cfg.clone(), http.clone(), providers::Source::Auto);

    // Favorites client + favorite-ID set (drives ★/☆ and v toggling).
    match favs::FavsClient::new(cfg) {
        Ok(client) => {
            match client.list().await {
                Ok(items) => {
                    let ids: HashSet<String> = items.into_iter().map(|f| f.id).collect();
                    picker.set_fav_ids(ids);
                }
                Err(e) => picker.set_status(format!("favorites unavailable: {e}")),
            }
            picker.set_favs_client(Some(client));
        }
        Err(e) => picker.set_status(format!("favorites unavailable: {e}")),
    }

    let start_tab = if favs_only {
        app::Tab::Favorites
    } else {
        app::Tab::Search
    };
    picker.start_on(start_tab);

    // Optional initial search: pre-fill the box and load page 0.
    let query = query.filter(|q| !q.trim().is_empty());
    if let Some(q) = &query {
        picker.set_query(q.clone());
        match providers::search_page(
            &http,
            cfg,
            providers::Source::Auto,
            q,
            app::PAGE_SIZE,
            &providers::PageCursor::Start,
        )
        .await
        {
            Ok(page) => {
                let items: Vec<app::UrlItem> =
                    page.results.into_iter().map(app::UrlItem::from).collect();
                let pager = app::Pager::search(
                    http.clone(),
                    cfg.clone(),
                    providers::Source::Auto,
                    q.clone(),
                    page.next,
                    page.total,
                );
                picker.set_search_page(items, pager);
            }
            Err(e) => picker.set_status(format!("search failed: {e}")),
        }
    } else if start_tab == app::Tab::Search {
        // Fresh search box: focus it so typing works immediately.
        picker.focus_search(true);
    }

    // Starting on Favorites: load page 0 up front.
    if start_tab == app::Tab::Favorites {
        if let Some(client) = picker.favs_client.clone() {
            match client.list_page(app::PAGE_SIZE, 0).await {
                Ok(p) => {
                    let items: Vec<app::UrlItem> =
                        p.items.into_iter().map(app::UrlItem::from).collect();
                    let pager = app::Pager::favs(client, p.total);
                    picker.set_favorites_page(items, pager);
                }
                Err(e) => picker.set_status(format!("favorites load failed: {e}")),
            }
        }
    }

    if let Some(url) = app::run(picker).await? {
        println!("{url}");
        clipboard::copy_text_quiet(&url);
    }
    Ok(())
}