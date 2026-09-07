# gifdeck

A standalone Rust + ratatui GIF picker for the command line. It searches
GIPHY and KLIPY, manages a favorites list synced through a self-hosted server
(`favs.veryshiny.net`), and feeds chosen GIFs into the Concord Discord TUI
client via Ctrl+V paste.

> MIT · © 2026 Diamond — this project reimplements the approach of gifgrep
> (MIT · © 2026 Peter Steinberger, steipete/gifgrep). See [LICENSE](LICENSE).

## Status

Session 2 (grid TUI). Implemented: CLI, config, GIPHY/KLIPY search providers,
favorites client, and an animated GIF grid TUI with inline previews on
Kitty/Ghostty-style terminals (Kitty graphics protocol) and a title-only
fallback elsewhere.

Not yet implemented (session 3): the unified Search/Favorites tabs, the
`c`/`v` favorite actions, and interactive search box.

## Usage

```
gifdeck search <query> [--source auto|giphy|klipy] [--max N] [--json]
gifdeck favs [--json]
gifdeck tui <query> [--favs]
```

Examples:

```
# Search KLIPY for "cat", print 2 direct GIF URLs
gifdeck search cat --source klipy --max 2

# Same, as JSON
gifdeck search cat --json

# List favorites from the server
gifdeck favs

# Animated GIF grid of search results
gifdeck tui cat

# Animated GIF grid of your favorites
gifdeck tui --favs
```

`gifdeck tui` with no query downloads nothing and prints a usage hint.

## TUI

The grid shows up to a bounded window (cache cap 32) of inline GIF previews
fetched and decoded asynchronously; the cells nearest the cursor load first
and older ones are evicted as you move. On terminals speaking the Kitty
graphics protocol (kitty, ghostty) the previews are animated. Anywhere else
the TUI falls back to a title-only grid with a footer note, so it never
crashes in a plain terminal emulator.

```
← → ↑ ↓ / h j k l   move (wrapping), PgUp/PgDn scroll within the page
d                    discard the page and fetch the next 50 results
u                    go back to the previous page (no-op on the first)
Enter / Space       select the GIF, printing its URL
q / Ctrl+C / Esc    quit
```

Paging hits the source API again (GIPHY offset / KLIPY cursor) or the
favorites server (`?limit=50&offset=…`), replaces the whole grid, and
resets the cursor to the top-left. The footer shows `page N/M` when the
source reports a total (GIPHY result count; favorites server
`X-Total-Count` header) and `page N` otherwise (KLIPY exposes no total).
At the end of the results the current page is kept and the footer notes
there are no more. Ctrl+D/Ctrl+U still work as aliases for d/u. On
terminals without a pager (not used in practice) u/d fall back to
half-page scrolling.

On select the URL is printed and copied to the clipboard when `wl-copy` or
`xclip` is installed. The TUI requires a terminal; over a non-TTY it exits
with an error.

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
- `src/app.rs` — TUI grid state, navigation, event loop, draw
- `src/preview.rs` — bounded LRU preview cache, async loader, kitty encode/render
- `src/cli.rs` — clap definitions