use clap::{Parser, Subcommand};

/// Search engine help text documenting config resolution.
pub const CONFIG_HELP: &str = "\
CONFIGURATION

Configuration is read once per process from the gifgrep config file:
    <dirs::config_dir()>/gifgrep/config.json
(overridable with the GIFDECK_CONFIG environment variable pointing at an
alternate file).

Keys (same names as gifgrep, so existing keys and tokens carry over):
    KLIPY_API_KEY             KLIPY search API key
    GIPHY_API_KEY             GIPHY search API key
    GIFGREP_FAVORITES_API     favorites server base URL
    GIFGREP_FAVORITES_TOKEN   favorites server auth token

Precedence: a non-empty environment variable of the same name wins over
the file value, which wins over an unset value. A missing file is fine
(empty config); invalid JSON is warned about on stderr and treated as
empty config. Unknown JSON keys are ignored. Values are never logged.";

#[derive(Debug, Parser)]
#[command(
    name = "gifdeck",
    version,
    about = "A GIF picker backed by GIPHY/KLIPY and a self-hosted favorites server",
    after_help = CONFIG_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Search KLIPY/GIPHY and print GIF URLs (one per line).
    Search {
        /// Search query.
        query: String,
        /// Source backend: auto, giphy, klipy, or tenor (klipy).
        #[arg(long, default_value = "auto")]
        source: String,
        /// Maximum number of results.
        #[arg(long, default_value_t = 20)]
        max: usize,
        /// Print results as a JSON array of objects instead of URLs.
        #[arg(long)]
        json: bool,
    },
    /// List favorites.
    Favs {
        /// Print favorites as a JSON array of objects instead of URLs.
        #[arg(long)]
        json: bool,
    },
    /// Open the interactive TUI GIF list.
    Tui {
        /// Optional search query; without one the list is empty.
        query: Option<String>,
        /// Start from the favorites list instead of search results.
        #[arg(long)]
        favs: bool,
    },
}