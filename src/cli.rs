use clap::{Parser, Subcommand};

/// Search engine help text documenting config resolution.
pub const CONFIG_HELP: &str = "\
CONFIGURATION

Configuration is read once per process from the gifdeck config file:
    <dirs::config_dir()>/gifdeck/config.json
(overridable with the GIFDECK_CONFIG environment variable pointing at an
alternate file).

Keys:
    KLIPY_API_KEY              KLIPY search API key
    GIPHY_API_KEY              GIPHY search API key
    GIFDECK_FAVORITES_API      favorites server base URL (server mode)
    GIFDECK_FAVORITES_TOKEN    favorites server auth token

Favorites storage is exclusive: with a token configured, favorites live on
the server; without one, they live in the local store at
<dirs::data_dir()>/gifdeck/favorites.json. The two never mix; the only
crossover is explicit: `gifdeck favs --export` copies the server's list
into the local store, `gifdeck favs --import` pushes the local store onto
the server (both require a configured server).

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
    /// List favorites (server when configured, otherwise the local store).
    Favs {
        /// Print favorites as a JSON array of objects instead of URLs.
        #[arg(long)]
        json: bool,
        /// Copy the SERVER's favorites into the local store (overwrites
        /// it). Requires a configured favorites server.
        #[arg(long, conflicts_with_all = ["json", "import"])]
        export: bool,
        /// Push the LOCAL store's favorites onto the server (upsert by
        /// id). Requires a configured favorites server.
        #[arg(long, conflicts_with_all = ["json", "export"])]
        import: bool,
    },
    /// Open the unified picker: a Search + Favorites tabbed GIF grid.
    Tui {
        /// Optional initial search query (pre-filled and run on open).
        query: Option<String>,
        /// Open on the Favorites tab instead of Search.
        #[arg(long)]
        favs: bool,
    },
}
