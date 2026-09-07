# gifdeck

A standalone Rust + ratatui GIF picker for the command line. It searches
GIPHY and KLIPY, manages a favorites list synced to local storage/self-hosted server.

## Usage

```
gifdeck                     # shorthand for `gifdeck tui`
gifdeck search <query> [--source auto|giphy|klipy] [--max N] [--json]
gifdeck favs [--json] [--export] [--import]
gifdeck tui [<query>] [--favs]
```

Examples:

```
# Search KLIPY for "cat", print 2 direct GIF URLs
gifdeck search cat --source klipy --max 2

# Same, as JSON
gifdeck search cat --json

# List favorites (server when configured, otherwise the local store)
gifdeck favs

# Copy the server's favorites into the local store (server mode only)
gifdeck favs --export

# Push the local store's favorites onto the server (server mode only)
gifdeck favs --import

# Unified picker: type a query, Enter to search, Tab to switch tabs
gifdeck tui

# Bare — same as `gifdeck tui`
gifdeck

# Same, but with the search pre-run for "cat"
gifdeck tui cat

# Open the picker directly on the Favorites tab
gifdeck tui --favs
```

`gifdeck tui` with no query opens on the Search tab with an empty, focused
search box — start typing.

## Favorites storage

Favorites have exactly one home, picked by the config — the two stores
are alternatives, never a fallback for one another:

- **Server mode** (default when `GIFDECK_FAVORITES_TOKEN` is configured,
  and requiring `GIFDECK_FAVORITES_API` to point at the server's base
  URL, e.g. `https://your-host/api/v1`; `X-Auth-Token` auth): favorites
  live on the self-hosted server.
- **Local mode** (no token configured, or no API base configured):
  favorites live in a plain JSON file at
  `~/.local/share/gifdeck/favorites.json` — no server needed, which is
  also gifdeck's out-of-the-box story.

The picker shows which mode it's in (`v favorite · local favorites` in
the footer) and `v` toggles against the active backend instantly. The
only crossover between the two stores is explicit:

- `gifdeck favs --export` — server → local store (overwrites the local
  file with the server's list)
- `gifdeck favs --import` — local store → server (upserts each entry by
  id; idempotent). Every favorite is attempted even after a failure;
  failed ones are listed per id with the reason, and the command exits
  non-zero when any failed.

So switching modes is a two-step choice: transfer with `--export`/`--import`,
then either set `"FAVORITES_MODE": "local"` in the config (keeping the
token for later), or remove `GIFDECK_FAVORITES_TOKEN`.

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
Enter               run the search / choose the selected GIF (prints its URL;
                    a chosen favorite gets its use count bumped)
Space               choose the selected GIF
Tab                 switch between Search and Favorites
c                   copy the GIF FILE to the clipboard as image/gif
y                   copy the GIF URL as text
v                   favorite / unfavorite the selected GIF (against the
                    configured backend — server or local store)
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
configured backend — on the server that's POST `/favorites` to star and
DELETE `/favorites/{id}` to unstar; in local mode it's an instant
upsert/remove in the JSON store. The outcome is reported in the footer
("★ saved to favorites" / "★ saved to local favorites"), and backend
errors surface there too and never crash the picker. While on the
Favorites tab the grid re-syncs with the backend after each toggle.

On select (Enter/Space) the URL is printed and copied to the clipboard
when `wl-copy` or `xclip` is installed. The TUI requires a terminal;
over a non-TTY it exits with an error.

### Use tracking

Using a *favorited* GIF bumps its use stats on the active backend
(`PATCH /favorites/{id}/use` on the server — `use_count += 1`,
`last_used = now`; a persisted update in the local store). What counts
as a use: picking (Enter/Space), copying the GIF file (`c`), and copying
the URL (`y`). Non-favorites are ignored, and a failed bump surfaces in
the footer without disturbing the pick/copy result.

Both backends present the favorites list in the same order — `use_count`
descending, then `last_used`, tie-broken by id (the server does it in
SQL; gifdeck applies the same ordering to the local store's view, and
`gifdeck favs` shows it too). On the Favorites tab the grid re-sorts
after each bump and the cursor follows the bumped item to its new spot,
so favorites you actually use float to the top right away.

## Configuration

Configuration is read once per process from the gifdeck config file:

```
<dirs::config_dir()>/gifdeck/config.json
```

Override the path with the `GIFDECK_CONFIG` environment variable pointing at
an alternate file. Keys (the config file is the only source of
configuration values):

| Key                      | Meaning                          |
| ------------------------ | -------------------------------- |
| `KLIPY_API_KEY`          | KLIPY search API key             |
| `GIPHY_API_KEY`          | GIPHY search API key             |
| `GIFDECK_FAVORITES_API`  | favorites server base URL (required in server mode; no default) |
| `GIFDECK_FAVORITES_TOKEN`| favorites server auth token      |
| `FAVORITES_MODE`         | explicit favorites mode: `server` \| `local` |

The token decides the favorites mode: set → server, unset → local store
(see [Favorites storage](#favorites-storage)). `FAVORITES_MODE` overrides
that heuristic explicitly: `server` forces server mode (even without a
token — requests go unauthenticated and errors surface in the footer),
`local` forces the local store (even with a token configured). An unknown
value warns on stderr and falls back to token-based mode.

A missing file is fine (empty config); invalid JSON prints a warning to
stderr and is treated as empty config. Unknown JSON keys are ignored, and
blank values count as unset. Values are never logged or printed.

All state lives under gifdeck-owned directories: config in
`~/.config/gifdeck/`, favorites store in `~/.local/share/gifdeck/`, and
temp GIF downloads for clipboard copies in `~/.cache/gifdeck/`.

## Building

```
cargo build --release
cargo test
```

## Layout

- `src/main.rs` — clap dispatch, unified-picker session setup
- `src/config.rs` — path resolution, load-once, precedence, favorites mode
- `src/providers.rs` — `GifResult` + GIPHY/KLIPY clients
- `src/favs.rs` — favorites backend (server client / local store), wiremock contract tests
- `src/store.rs` — local favorites store (atomic JSON, `~/.local/share/gifdeck/`)
- `src/app.rs` — tabbed picker state, search box, keymap, event loop, draw
- `src/clipboard.rs` — GIF-file/URL clipboard (wl-copy, xclip)
- `src/preview.rs` — bounded LRU preview cache, async loader, kitty encode/render
- `src/cli.rs` — clap definitions