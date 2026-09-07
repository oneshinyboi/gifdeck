# gifdeck

A standalone Rust + ratatui GIF picker for the command line. It searches
GIPHY and KLIPY, manages a favorites list synced through a self-hosted server
(`favs.veryshiny.net`), and feeds chosen GIFs into the Concord Discord TUI
client via Ctrl+V paste.

> MIT · © 2026 Diamond — this project reimplements the approach of gifgrep
> (MIT · © 2026 Peter Steinberger, steipete/gifgrep). See [LICENSE](LICENSE).

## Status

Session 1 (scaffold + plumbing). Implemented: CLI, config, GIPHY/KLIPY search
providers, favorites client, and a minimal read-only ratatui list UI.

Not yet implemented (later sessions): grid layout, animated previews, the
unified Search/Favorites GUI, and the `c`/`v` favorite actions.

## Usage

```
gifdeck search <query> [--source auto|giphy|klipy] [--max N] [--json]
gifdeck favs [--json]
gifdeck tui [query] [--favs]
```

Examples:

```
# Search KLIPY for "cat", print 2 direct GIF URLs
gifdeck search cat --source klipy --max 2

# Same, as JSON
gifdeck search cat --json

# List favorites from the server
gifdeck favs

# Interactive list TUI (search results, or favorites with --favs)
gifdeck tui cat
gifdeck tui --favs
```

In the TUI: `↑`/`k` and `↓`/`j` scroll, `Enter` selects (prints the URL and
copies it to the clipboard when `wl-copy` or `xclip` is installed), `q`
quits. The TUI requires a terminal; over a non-TTY it exits with an error.

## Configuration

Configuration is read once per process from the gifgrep config file:

```
<dirs::config_dir()>/gifgrep/config.json
```

Override the path with the `GIFDECK_CONFIG` environment variable pointing at
an alternate file.

Keys (same names as gifgrep, so existing keys and tokens carry over):

| Key                      | Meaning                          |
| ------------------------ | -------------------------------- |
| `KLIPY_API_KEY`          | KLIPY search API key             |
| `GIPHY_API_KEY`          | GIPHY search API key             |
| `GIFGREP_FAVORITES_API`  | favorites server base URL        |
| `GIFGREP_FAVORITES_TOKEN`| favorites server auth token      |

Precedence: a non-empty environment variable of the same name wins over the
file value, which wins over an unset value. A missing file is fine (empty
config); invalid JSON prints a warning to stderr and is treated as empty
config. Unknown JSON keys are ignored. Values are never logged or printed.

## Building

```
cargo build --release
cargo test
```

## Layout

- `src/main.rs` — clap dispatch
- `src/config.rs` — path resolution, load-once, precedence
- `src/providers.rs` — `GifResult` + GIPHY/KLIPY clients
- `src/favs.rs` — self-hosted favorites client (wiremock contract tests)
- `src/app.rs` — TUI App state, event loop, draw (list only)
- `src/cli.rs` — clap definitions