use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "gifdeck",
    version,
    about = "A GIF picker backed by GIPHY/KLIPY"
)]
pub struct Cli {
    /// No subcommand (bare `gifdeck`) launches the TUI picker.
    #[command(subcommand)]
    pub command: Option<Command>,
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
