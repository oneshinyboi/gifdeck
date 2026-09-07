# gifdeck

A standalone Rust + ratatui GIF picker for the command line. It searches
GIPHY and KLIPY, manages a favorites list synced through a self-hosted server
(`favs.veryshiny.net`), and feeds chosen GIFs into the Concord Discord TUI
client via Ctrl+V paste.

> MIT · © 2026 Diamond — this project reimplements the approach of gifgrep
> (MIT · © 2026 Peter Steinberger, steipete/gifgrep). See [LICENSE](LICENSE).

## Status

Session 4 (unified picker). Implemented: CLI, config, GIPHY/KLIPY search
providers, favorites client, and a single tabbed picker — an interactive
Search box plus a Favorites tab sharing one animated GIF grid with inline
previews on Kitty/Ghostty-style terminals (Kitty graphics protocol) and a
title-only fallback elsewhere. `v` favorites/unfavorites against the server,
`c` copies the GIF file itself to the clipboard as `image/gif` (real
attachment on paste), `y` copies the URL.

Not yet implemented (next session): local favorites cache and a
server/local mode; public readiness.

## Usage

```
gifdeck search <query> [--source auto|giphy|klipy] [--max N] [--json]
gifdeck favs [--json]
gifdeck tui [<query>] [--favs]
```

Examples:

```
# Search KLIPY for "cat", print 2 direct GIF URLs
gifdeck search cat --source klipy --max 2

# Same, as JSON
gifdeck search cat --json

# List favorites from the server
gifdeck favs

# Unified picker: type a query, Enter to search, Tab to switch tabs
gifdeck tui

# Same, but with the search pre-run for "cat"
gifdeck tui cat

# Open the picker directly on the Favorites tab
gifdeck tui --favs
```

`gifdeck tui` with no query opens on the Search tab with an empty, focused
search box — start typing.

## TUI

```
Tab: [Search] [Favorites]  q quit
 > type a query… (/ focus, Enter search)
 ☆ 0001 · title   ★ 0002 · title   …
 12/50 · page 1/3 · u/d page · Tab switch · c copy gif · y copy url · v favorite
```

The two tabs share one grid; switching swaps the underlying items and
pager while keeping each tab's cursor and page. Cells show ★ when the GIF
is a server favorite and ☆ otherwise.

The grid shows up to a bounded window (cache cap 32) of inline GIF previews
fetched and decoded asynchronously; the cells nearest the cursor load first
and older ones are evicted as you move. On terminals speaking the Kitty
graphics protocol (kitty, ghostty) the previews are animated. Anywhere else
the TUI falls back to a title-only grid with a footer note, so it never
crashes in a plain terminal emulator.

```
← → ↑ ↓ / h j k l   move (wrapping)
d / u               next / previous page (the ONLY paging keys —
                    PageUp/PageDown are intentionally not bound)
/                   focus the search box (Search tab)
(type…)             edit the query — Backspace deletes, Ctrl+U clears,
                    Esc clears then blurs; search runs on Enter only
Enter               run the search / choose the selected GIF (prints its URL)
Space               choose the selected GIF
Tab                 switch between Search and Favorites
c                   copy the GIF FILE to the clipboard as image/gif
y                   copy the GIF URL as text
v                   favorite / unfavorite the selected GIF on the server
q / Esc / Ctrl+C    quit (Esc clears/blurs the search box when focused)
```

Paging (d/u) hits the source API again (GIPHY offset / KLIPY cursor) or
the favorites server (`?limit=50&offset=…`), replaces the whole grid, and
resets the cursor to the top-left. The footer shows `page N/M` when the
source reports a total (GIPHY result count; favorites server
`X-Total-Count` header) and `page N` otherwise (KLIPY exposes no total).
At the end of the results the current page is kept and the footer notes
there are no more. Ctrl+D/Ctrl+U still work as aliases for d/u. On
terminals without a pager (not used in practice) u/d fall back to
half-page scrolling.

### Clipboard

`c` downloads the selected GIF (10s timeout) to a temp file under
`~/.cache/gifdeck/`, then puts the bytes on the clipboard as `image/gif`
so a Ctrl+V in Concord pastes a real animated .gif attachment:

- Wayland: `wl-copy --type image/gif < tempfile`
- fallback: `xclip -selection clipboard -t image/gif -i tempfile`

The temp file is removed right after the clipboard helper takes over (it
keeps serving from its open descriptor). `y` copies just the URL text
(the "paste URL → animated embed" path): `wl-copy <url>` or
`xclip -selection clipboard` reading the URL from stdin.

`v` toggles the favorite state of the selected GIF against the
self-hosted server (POST `/favorites` to star, `DELETE /favorites/{id}`
to unstar) and reports the outcome in the footer; server errors surface
there too and never crash the picker. While on the Favorites tab the
grid re-syncs with the server after each toggle.

On select (Enter/Space) the URL is printed and copied to the clipboard
when `wl-copy` or `xclip` is installed. The TUI requires a terminal;
over a non-TTY it exits with an error.

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

- `src/main.rs` — clap dispatch, unified-picker session setup
- `src/config.rs` — path resolution, load-once, precedence
- `src/providers.rs` — `GifResult` + GIPHY/KLIPY clients
- `src/favs.rs` — self-hosted favorites client (wiremock contract tests)
- `src/app.rs` — tabbed picker state, search box, keymap, event loop, draw
- `src/clipboard.rs` — GIF-file/URL clipboard (wl-copy, xclip)
- `src/preview.rs` — bounded LRU preview cache, async loader, kitty encode/render
- `src/cli.rs` — clap definitions