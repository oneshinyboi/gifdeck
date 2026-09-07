use std::collections::HashSet;
use std::io::{self, IsTerminal};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use ratatui_image::picker::{Picker, ProtocolType};

use crate::clipboard;
use crate::config::Config;
use crate::favs::{FavItem, FavsBackend};
use crate::preview::{self, PreviewCache, PreviewLoader, MAX_CACHE_CAP, PREVIEW_SIZE};
use crate::providers::{self, GifResult, Provider};

/// A selectable GIF: what's shown vs. what gets pasted when selected.
#[derive(Debug, Clone)]
pub struct UrlItem {
    pub id: String,
    pub title: String,
    pub url: String,
    pub preview_url: String,
    pub provider: Provider,
}

impl From<GifResult> for UrlItem {
    fn from(r: GifResult) -> Self {
        let title = if r.title.is_empty() {
            r.id.clone()
        } else {
            r.title
        };
        UrlItem {
            id: r.id,
            title,
            url: r.url,
            preview_url: r.preview_url,
            provider: r.provider,
        }
    }
}

impl From<FavItem> for UrlItem {
    fn from(f: FavItem) -> Self {
        let title = if f.title.is_empty() {
            f.id.clone()
        } else {
            f.title
        };
        UrlItem {
            id: f.id,
            title,
            url: f.url,
            preview_url: f.preview,
            provider: Provider::from_label(&f.provider),
        }
    }
}

impl UrlItem {
    /// Reconstruct the `GifResult` needed to save this item as a favorite
    /// (works for both search results and server favorites).
    pub fn to_gif_result(&self) -> GifResult {
        GifResult {
            id: self.id.clone(),
            title: self.title.clone(),
            url: self.url.clone(),
            preview_url: self.preview_url.clone(),
            provider: self.provider,
        }
    }
}

/// Minimum width of one grid cell (PREVIEW_WIDTH + 2 border columns).
pub const MIN_CELL_W: u16 = PREVIEW_SIZE.width + 2;
/// Extra rows per cell besides the image: 2 borders + 1 title line.
const CELL_EXTRA_H: u16 = 3;
/// Items per page when paging against a server (u / d).
pub const PAGE_SIZE: usize = 50;

/// The two tabs of the unified picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Search,
    Favorites,
}

impl Tab {
    pub fn toggle(self) -> Self {
        match self {
            Tab::Search => Tab::Favorites,
            Tab::Favorites => Tab::Search,
        }
    }
}

/// Per-tab grid state: items, cursor, and the server pager backing u/d.
#[derive(Debug, Default)]
pub struct GridState {
    pub items: Vec<UrlItem>,
    pub selected: usize,
    pub first_item: usize,
    /// 0-based index of the currently displayed page.
    pub page: usize,
    pub pager: Option<Pager>,
}

/// Deferred work queued by `handle_key` and executed by the event loop
/// (all of it needs the network, so none of it can run synchronously).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// Fetch the neighbor page (d / u).
    Page(PageDir),
    /// Run a fresh search for the current query (Enter in the search box).
    Search,
    /// Toggle the favorite state of the selected item (v).
    Toggle,
    /// Download the selected GIF and copy it as image/gif (c).
    CopyGif,
    /// Copy the selected GIF's URL as text (y).
    CopyUrl,
    /// Tab was switched: refresh favorite IDs, load the favorites page if
    /// the tab has never been populated.
    TabLoad,
}

/// Direction of a page turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageDir {
    Next,
    Prev,
}

/// Outcome of a page-turn attempt.
#[derive(Debug)]
pub enum PageTurn {
    /// The neighbor page was fetched; the grid should be replaced by these.
    Page(Vec<UrlItem>),
    /// Already on the first page (u).
    AtFirst,
    /// No further results exist (d).
    AtLast,
}

/// Server-backed paging: u/d replace the whole grid with the
/// previous/next page of results instead of scrolling within a page.
#[derive(Debug, Clone)]
pub enum Pager {
    Search {
        client: reqwest::Client,
        cfg: Config,
        source: providers::Source,
        query: String,
        /// Cursor used to fetch each visited page; `back[0]` is always
        /// `Start`. `back.len() == current page + 1`.
        back: Vec<providers::PageCursor>,
        /// Cursor for the page after the current one (`None` = exhausted).
        next: Option<providers::PageCursor>,
        /// Total matching items, when the provider reports it (GIPHY).
        total: Option<usize>,
    },
    Favs {
        backend: FavsBackend,
        /// Total favorites, from the server's `X-Total-Count` (or the
        /// local store's length).
        total: Option<usize>,
    },
}

/// Pages covered by `total` items at `PAGE_SIZE` per page.
fn total_pages(total: usize) -> usize {
    total.div_ceil(PAGE_SIZE)
}

/// Footer fragment for the current page, e.g. `page 2/7` (or `page 2`
/// when the total is unknown).
fn page_label(page: usize, total: Option<usize>) -> String {
    let pages = total.map(total_pages);
    match pages {
        Some(p) if p > 0 => format!("page {}/{}", page + 1, p),
        _ => format!("page {}", page + 1),
    }
}

impl Pager {
    /// Pager for search results. `next`/`total` come from the initial
    /// fetch (page 0).
    pub fn search(
        client: reqwest::Client,
        cfg: Config,
        source: providers::Source,
        query: String,
        next: Option<providers::PageCursor>,
        total: Option<usize>,
    ) -> Self {
        Pager::Search {
            client,
            cfg,
            source,
            query,
            back: vec![providers::PageCursor::Start],
            next,
            total,
        }
    }

    /// Pager for favorites (offset-based): the self-hosted server or the
    /// local store, behind the same backend. `total` comes from the
    /// initial fetch (`X-Total-Count` / store length).
    pub fn favs(backend: FavsBackend, total: Option<usize>) -> Self {
        Pager::Favs { backend, total }
    }

    /// Total items across all pages, when the source reports it.
    pub fn total(&self) -> Option<usize> {
        match self {
            Pager::Search { total, .. } | Pager::Favs { total, .. } => *total,
        }
    }

    /// Fetch the neighbor page in direction `dir`, given the current
    /// 0-based page number. Internal cursor state is only committed when
    /// the fetch succeeds.
    pub async fn turn(&mut self, dir: PageDir, page: usize) -> Result<PageTurn> {
        match self {
            Pager::Search {
                client,
                cfg,
                source,
                query,
                back,
                next,
                total,
            } => {
                let cursor = match dir {
                    PageDir::Next => {
                        // With a known total, refuse turns past the last
                        // page without hitting the API.
                        if let Some(t) = *total {
                            if page + 1 >= total_pages(t) {
                                return Ok(PageTurn::AtLast);
                            }
                        }
                        match next.clone() {
                            Some(c) => c,
                            None => return Ok(PageTurn::AtLast),
                        }
                    }
                    PageDir::Prev => {
                        if back.len() < 2 {
                            return Ok(PageTurn::AtFirst);
                        }
                        back.pop();
                        back.last()
                            .cloned()
                            .expect("back is non-empty after pop guard")
                    }
                };
                let fetched =
                    providers::search_page(client, cfg, *source, query, PAGE_SIZE, &cursor)
                        .await?;
                if fetched.results.is_empty() {
                    // Restore the cursor we popped so d still works after.
                    if matches!(dir, PageDir::Prev) {
                        back.push(cursor);
                    }
                    return Ok(PageTurn::AtLast);
                }
                let next_cursor = fetched.next;
                let fetched_total = fetched.total;
                let items: Vec<UrlItem> = fetched.results.into_iter().map(UrlItem::from).collect();
                match dir {
                    PageDir::Next => {
                        back.push(cursor);
                        *next = next_cursor;
                    }
                    PageDir::Prev => *next = next_cursor,
                }
                *total = fetched_total.or(*total);
                Ok(PageTurn::Page(items))
            }
            Pager::Favs { backend, total } => {
                let target = match dir {
                    PageDir::Next => {
                        if let Some(t) = *total {
                            if page + 1 >= total_pages(t) {
                                return Ok(PageTurn::AtLast);
                            }
                        }
                        page + 1
                    }
                    PageDir::Prev => {
                        if page == 0 {
                            return Ok(PageTurn::AtFirst);
                        }
                        page - 1
                    }
                };
                let fetched = backend.list_page(PAGE_SIZE, target * PAGE_SIZE).await?;
                *total = fetched.total.or(*total);
                if fetched.items.is_empty() {
                    return Ok(PageTurn::AtLast);
                }
                let items = fetched
                    .items
                    .into_iter()
                    .map(UrlItem::from)
                    .collect();
                Ok(PageTurn::Page(items))
            }
        }
    }
}

fn cell_height() -> u16 {
    PREVIEW_SIZE.height + CELL_EXTRA_H
}

/// Columns that fit in a grid area of the given width.
pub fn grid_cols(width: u16) -> u16 {
    (width / MIN_CELL_W).max(1)
}

/// Number of fully-visible rows in a grid area of the given height.
pub fn visible_rows(area_height: u16) -> u16 {
    (area_height / cell_height()).max(1)
}

/// Wrap navigation inside a flat index list (left arrow).
pub fn nav_left(i: usize, len: usize) -> usize {
    if len <= 1 {
        0
    } else {
        (i + len - 1) % len
    }
}

/// Wrap navigation inside a flat index list (right arrow).
pub fn nav_right(i: usize, len: usize) -> usize {
    if len <= 1 {
        0
    } else {
        (i + 1) % len
    }
}

/// Move one row up in a grid, wrapping to the matching column of the last row.
pub fn nav_up(i: usize, cols: usize, len: usize) -> usize {
    if len <= 1 || cols == 0 {
        return 0;
    }
    if i >= cols {
        i - cols
    } else {
        let last_row = (len - 1) / cols;
        let target = last_row * cols + (i % cols);
        if target >= len {
            len - 1
        } else {
            target
        }
    }
}

/// Move one row down in a grid, wrapping to the matching column of the first row.
pub fn nav_down(i: usize, cols: usize, len: usize) -> usize {
    if len <= 1 || cols == 0 {
        return 0;
    }
    if i + cols < len {
        i + cols
    } else {
        i % cols
    }
}

/// Indices of the cells in the visible window
/// (`first_item..first_item + cols*rows`), ordered by increasing Euclidean
/// distance from the cursor. Stops at the end of `len` on a partial last row.
pub fn nearest_slots(
    first_item: usize,
    cols: usize,
    rows: usize,
    len: usize,
    selected: usize,
) -> Vec<usize> {
    let cols = cols.max(1);
    let rows = rows.max(1);
    let mut slots: Vec<usize> = (0..cols.saturating_mul(rows))
        .map(|slot| first_item + slot)
        .filter(|&idx| idx < len)
        .collect();
    let sel_r = selected / cols;
    let sel_c = selected % cols;
    slots.sort_by_key(|&idx| {
        let r = idx / cols;
        let c = idx % cols;
        let dr = r.abs_diff(sel_r);
        let dc = c.abs_diff(sel_c);
        dr.saturating_mul(dr) + dc.saturating_mul(dc)
    });
    slots
}

pub enum PreviewMode {
    Graphics {
        cache: Arc<Mutex<PreviewCache>>,
        loader: PreviewLoader,
    },
    Fallback,
}

pub struct App {
    /// Active tab.
    pub tab: Tab,
    /// Search-tab grid state.
    pub search: GridState,
    /// Favorites-tab grid state.
    pub favorites: GridState,
    /// Search-box text (typed on the Search tab, run on Enter).
    pub query: String,
    /// Whether typing goes into the search box instead of the keymap.
    /// Only ever true on the Search tab.
    pub search_focused: bool,
    /// IDs of the current favorites (drives ★/☆ and v toggling).
    pub fav_ids: HashSet<String>,
    /// Where favorites live: the self-hosted server, or the local store.
    pub favs: FavsBackend,
    http: reqwest::Client,
    cfg: Config,
    source: providers::Source,
    cols: u16,
    rows: u16,
    should_quit: bool,
    selected_url: Option<String>,
    mode: PreviewMode,
    /// Transient footer message (page-turn feedback, load errors).
    pub status: Option<String>,
    /// Action requested via keypress, executed by the event loop.
    pub pending: Option<Pending>,
}

impl App {
    pub fn new(
        cfg: Config,
        http: reqwest::Client,
        source: providers::Source,
        favs: FavsBackend,
    ) -> Self {
        App {
            tab: Tab::Search,
            search: GridState::default(),
            favorites: GridState::default(),
            query: String::new(),
            search_focused: false,
            fav_ids: HashSet::new(),
            favs,
            http,
            cfg,
            source,
            cols: 1,
            rows: 1,
            should_quit: false,
            selected_url: None,
            mode: PreviewMode::Fallback,
            status: None,
            pending: None,
        }
    }

    /// Which tab the picker opens on.
    pub fn start_on(&mut self, tab: Tab) {
        self.tab = tab;
    }

    pub fn set_query(&mut self, q: impl Into<String>) {
        self.query = q.into();
    }

    pub fn set_fav_ids(&mut self, ids: HashSet<String>) {
        self.fav_ids = ids;
    }

    pub fn focus_search(&mut self, focused: bool) {
        self.search_focused = focused;
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
    }

    /// Install the first page of search results plus its pager.
    pub fn set_search_page(&mut self, items: Vec<UrlItem>, pager: Pager) {
        self.search.items = items;
        self.search.pager = Some(pager);
        self.search.page = 0;
        self.search.selected = 0;
        self.search.first_item = 0;
    }

    /// Install the first page of favorites plus its pager.
    pub fn set_favorites_page(&mut self, items: Vec<UrlItem>, pager: Pager) {
        self.favorites.items = items;
        self.favorites.pager = Some(pager);
        self.favorites.page = 0;
        self.favorites.selected = 0;
        self.favorites.first_item = 0;
    }

    /// Grid state of the active tab.
    pub fn grid(&self) -> &GridState {
        match self.tab {
            Tab::Search => &self.search,
            Tab::Favorites => &self.favorites,
        }
    }

    fn grid_mut(&mut self) -> &mut GridState {
        match self.tab {
            Tab::Search => &mut self.search,
            Tab::Favorites => &mut self.favorites,
        }
    }

    fn active_item(&self) -> Option<&UrlItem> {
        let g = self.grid();
        g.items.get(g.selected)
    }

    pub fn enable_previews(&mut self, cache: Arc<Mutex<PreviewCache>>, loader: PreviewLoader) {
        self.mode = PreviewMode::Graphics { cache, loader };
    }

    /// Keep `selected` inside the visible window while staying row-aligned.
    fn ensure_visible(&mut self) {
        let cols = (self.cols as usize).max(1);
        let rows = (self.rows as usize).max(1);
        let visible = rows.saturating_mul(cols);
        let len = self.grid().items.len();
        if len == 0 {
            return;
        }
        let g = self.grid_mut();
        if g.selected < g.first_item {
            g.first_item = (g.selected / cols) * cols;
        } else if g.selected >= g.first_item + visible {
            let sel_row = g.selected / cols;
            let new_first_row = sel_row.saturating_sub(rows.saturating_sub(1));
            g.first_item = new_first_row * cols;
        }
        let max_first = (len.saturating_sub(1) / cols) * cols;
        if g.first_item > max_first {
            g.first_item = max_first;
        }
    }

    fn choose(&mut self) {
        if let Some(item) = self.active_item() {
            self.selected_url = Some(item.url.clone());
            self.should_quit = true;
        }
    }

    fn step(&mut self, f: impl Fn(usize) -> usize) {
        let cur = self.grid().selected;
        self.grid_mut().selected = f(cur);
        self.ensure_visible();
    }

    fn scroll_down(&mut self, rows: usize) {
        let len = self.grid().items.len();
        if len == 0 {
            return;
        }
        let jump = rows.max(1).saturating_mul(self.cols.max(1) as usize);
        let cur = self.grid().selected;
        self.grid_mut().selected = if cur + jump >= len { len - 1 } else { cur + jump };
        self.ensure_visible();
    }

    fn scroll_up(&mut self, rows: usize) {
        let len = self.grid().items.len();
        if len == 0 {
            return;
        }
        let jump = rows.max(1).saturating_mul(self.cols.max(1) as usize);
        let cur = self.grid().selected;
        self.grid_mut().selected = if cur < jump { 0 } else { cur - jump };
        self.ensure_visible();
    }

    fn half_page_down(&mut self) {
        self.scroll_down(((self.rows.max(1) / 2) as usize).max(1));
    }

    fn half_page_up(&mut self) {
        self.scroll_up(((self.rows.max(1) / 2) as usize).max(1));
    }

    fn clear_previews(&mut self) {
        if let PreviewMode::Graphics { cache, .. } = &self.mode {
            cache.lock().unwrap().clear();
        }
    }

    /// Swap the active grid for a new page of items: reset the cursor to
    /// the top-left and drop cached previews of the replaced page.
    fn apply_page(&mut self, items: Vec<UrlItem>) {
        let g = self.grid_mut();
        g.items = items;
        g.selected = 0;
        g.first_item = 0;
        self.status = None;
        self.clear_previews();
    }

    /// Fetch and apply the neighbor page in direction `dir`, or surface
    /// why the turn was refused. The current page is kept on failure.
    async fn load_page(&mut self, dir: PageDir) {
        let page = self.grid().page;
        let mut pager = match std::mem::take(&mut self.grid_mut().pager) {
            Some(p) => p,
            None => return,
        };
        let turn = pager.turn(dir, page).await;
        self.grid_mut().pager = Some(pager);
        match turn {
            Ok(PageTurn::Page(items)) => {
                self.grid_mut().page = match dir {
                    PageDir::Next => page + 1,
                    PageDir::Prev => page.saturating_sub(1),
                };
                self.apply_page(items);
            }
            Ok(PageTurn::AtFirst) => self.status = Some("already on the first page".into()),
            Ok(PageTurn::AtLast) => self.status = Some("no more results".into()),
            Err(e) => self.status = Some(format!("page load failed: {e}")),
        }
    }

    /// Run a fresh search for the current query and replace the Search
    /// tab's grid. Errors surface in the footer; the old grid is kept.
    async fn run_search(&mut self) {
        let q = self.query.trim().to_string();
        if q.is_empty() {
            self.status = Some("type a query, then Enter".into());
            return;
        }
        match providers::search_page(
            &self.http,
            &self.cfg,
            self.source,
            &q,
            PAGE_SIZE,
            &providers::PageCursor::Start,
        )
        .await
        {
            Ok(page) => {
                let n = page.results.len();
                let items: Vec<UrlItem> = page.results.into_iter().map(UrlItem::from).collect();
                let pager = Pager::search(
                    self.http.clone(),
                    self.cfg.clone(),
                    self.source,
                    q.clone(),
                    page.next,
                    page.total,
                );
                self.search.pager = Some(pager);
                self.search.page = 0;
                self.tab = Tab::Search;
                self.apply_page(items);
                self.status = if n == 0 {
                    Some("no results".into())
                } else {
                    Some(format!("{n} results"))
                };
            }
            Err(e) => self.status = Some(format!("search failed: {e}")),
        }
    }

    /// Toggle the favorite state of the selected item against the
    /// configured backend (server or local store): delete when already
    /// favorited, save otherwise. The footer reports the outcome; errors
    /// never crash the picker.
    async fn toggle_favorite(&mut self) {
        let Some(item) = self.active_item().cloned() else {
            return;
        };
        let favs = self.favs.clone();
        let was_fav = self.fav_ids.contains(&item.id);
        let result = if was_fav {
            favs.delete(&item.id).await
        } else {
            favs.save(&item.to_gif_result()).await.map(|_| ())
        };
        match result {
            Ok(()) => {
                let local = self.favs.is_local();
                if was_fav {
                    self.fav_ids.remove(&item.id);
                    self.status = Some(if local {
                        "☆ removed from local favorites".into()
                    } else {
                        "☆ removed from favorites".into()
                    });
                } else {
                    self.fav_ids.insert(item.id.clone());
                    self.status = Some(if local {
                        "★ saved to local favorites".into()
                    } else {
                        "★ saved to favorites".into()
                    });
                }
                if self.tab == Tab::Favorites {
                    self.refresh_favorites_page().await;
                }
            }
            Err(e) => self.status = Some(format!("favorite failed: {e}")),
        }
    }

    /// Re-fetch the currently displayed favorites page so the grid stays
    /// in sync after a toggle; keeps the cursor when possible and steps
    /// back a page when the current one emptied out.
    async fn refresh_favorites_page(&mut self) {
        let favs = self.favs.clone();
        let mut page = self.favorites.page;
        let fetched = loop {
            match favs.list_page(PAGE_SIZE, page * PAGE_SIZE).await {
                Ok(p) if p.items.is_empty() && page > 0 => page -= 1,
                other => break other,
            }
        };
        let Ok(fetched) = fetched else {
            return;
        };
        let selected = self.favorites.selected;
        let total = fetched.total;
        let items: Vec<UrlItem> = fetched.items.into_iter().map(UrlItem::from).collect();
        let len = items.len();
        self.favorites.page = page;
        self.favorites.items = items;
        self.favorites.selected = if len == 0 { 0 } else { selected.min(len - 1) };
        self.favorites.first_item = 0;
        if let Some(Pager::Favs { total: pt, .. }) = self.favorites.pager.as_mut() {
            if total.is_some() {
                *pt = total;
            }
        }
    }

    /// Download the selected GIF and put its bytes on the clipboard as
    /// image/gif (paste inserts a real animated GIF attachment).
    async fn copy_gif(&mut self) {
        let Some(item) = self.active_item().cloned() else {
            return;
        };
        match clipboard::copy_gif_file(&self.http, &item.url).await {
            Ok(()) => self.status = Some("copied GIF to clipboard".into()),
            Err(e) => self.status = Some(format!("copy failed: {e}")),
        }
    }

    /// Copy the selected GIF's URL as text (the "paste URL → animated
    /// embed" path).
    fn copy_url(&mut self) {
        let Some(item) = self.active_item().cloned() else {
            return;
        };
        match clipboard::copy_text(&item.url) {
            Ok(()) => self.status = Some("copied URL".into()),
            Err(e) => self.status = Some(format!("copy failed: {e}")),
        }
    }

    /// After a tab switch: refresh the favorite-ID set (star markers) and
    /// load the favorites page if the tab has never been populated.
    async fn load_tab(&mut self) {
        let favs = self.favs.clone();
        match favs.list().await {
            Ok(items) => {
                self.fav_ids = items.into_iter().map(|f| f.id).collect();
            }
            Err(e) => self.status = Some(format!("favorites unavailable: {e}")),
        }
        if self.tab == Tab::Favorites && self.favorites.pager.is_none() {
            match favs.list_page(PAGE_SIZE, 0).await {
                Ok(p) => {
                    let empty = p.items.is_empty();
                    let total = p.total;
                    let items: Vec<UrlItem> = p.items.into_iter().map(UrlItem::from).collect();
                    self.favorites.pager = Some(Pager::favs(favs, total));
                    self.favorites.page = 0;
                    self.apply_page(items);
                    if empty {
                        self.status = None;
                    }
                }
                Err(e) => self.status = Some(format!("favorites load failed: {e}")),
            }
        }
        self.clear_previews();
    }

    fn switch_tab(&mut self) {
        self.tab = self.tab.toggle();
        self.search_focused = false;
        self.pending = Some(Pending::TabLoad);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl+C always quits, even while typing in the search box.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        if self.search_focused {
            self.handle_search_key(key);
            return;
        }
        let len = self.grid().items.len();
        match key.code {
            KeyCode::Tab => self.switch_tab(),
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char(' ') => self.choose(),
            KeyCode::Char('j') | KeyCode::Down => {
                let cols = self.cols.max(1) as usize;
                self.step(|i| nav_down(i, cols, len));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let cols = self.cols.max(1) as usize;
                self.step(|i| nav_up(i, cols, len));
            }
            KeyCode::Char('h') | KeyCode::Left => self.step(|i| nav_left(i, len)),
            KeyCode::Char('l') | KeyCode::Right => self.step(|i| nav_right(i, len)),
            // Plain d/u page the grid; Ctrl+D/Ctrl+U still work as aliases
            // (the code is Char('d')/'u' either way).
            KeyCode::Char('d') => {
                if self.grid().pager.is_some() {
                    self.pending = Some(Pending::Page(PageDir::Next));
                } else {
                    self.half_page_down();
                }
            }
            KeyCode::Char('u') => {
                if self.grid().pager.is_some() {
                    self.pending = Some(Pending::Page(PageDir::Prev));
                } else {
                    self.half_page_up();
                }
            }
            KeyCode::Char('/') if self.tab == Tab::Search => self.search_focused = true,
            KeyCode::Char('v') => {
                if self.active_item().is_some() {
                    self.pending = Some(Pending::Toggle);
                }
            }
            KeyCode::Char('c') => {
                if self.active_item().is_some() {
                    self.pending = Some(Pending::CopyGif);
                }
            }
            KeyCode::Char('y') => {
                if self.active_item().is_some() {
                    self.pending = Some(Pending::CopyUrl);
                }
            }
            KeyCode::Home => {
                self.grid_mut().selected = 0;
                self.ensure_visible();
            }
            KeyCode::End => {
                self.grid_mut().selected = len.saturating_sub(1);
                self.ensure_visible();
            }
            _ => {}
        }
    }

    /// Keys while the search box is focused: printable characters edit
    /// the query, Enter runs the search, Ctrl+U clears, Esc clears/blurs,
    /// Tab switches tabs. Nothing reaches the grid keymap.
    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.search_focused = false;
                if self.query.trim().is_empty() {
                    self.status = Some("type a query, then Enter".into());
                } else {
                    self.pending = Some(Pending::Search);
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Tab => {
                self.search_focused = false;
                self.switch_tab();
            }
            KeyCode::Esc => {
                if self.query.is_empty() {
                    self.search_focused = false;
                } else {
                    self.query.clear();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear()
            }
            KeyCode::Char(c)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
            }
            _ => {}
        }
    }

    /// Ask the loader to fetch previews for the cells nearest the cursor,
    /// in order of increasing distance. The cache cap is sized to the visible
    /// grid, so every visible cell is fetched and none is evicted while shown.
    fn request_previews(&self) {
        let (cache, loader) = match &self.mode {
            PreviewMode::Graphics { cache, loader } => (cache, loader),
            PreviewMode::Fallback => return,
        };
        let cols = (self.cols as usize).max(1);
        let rows = (self.rows as usize).max(1);
        for idx in nearest_slots(
            self.grid().first_item,
            cols,
            rows,
            self.grid().items.len(),
            self.grid().selected,
        ) {
            let url = &self.grid().items[idx].preview_url;
            if !url.is_empty() {
                preview::ensure_requested(cache, loader, url);
            }
        }
    }

    /// Draw the picker onto the provided frame.
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let [header_area, grid_area, footer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(2),
            ])
            .areas(area);

        self.cols = grid_cols(grid_area.width);
        self.rows = visible_rows(grid_area.height);
        self.ensure_visible();

        if let PreviewMode::Graphics { cache, .. } = &self.mode {
            // Headroom (2×) so an overshoot never immediately evicts an
            // on-screen entry (which would reload it, thrashing the counter).
            let visible = (self.cols as usize).saturating_mul(self.rows as usize);
            cache
                .lock()
                .unwrap()
                .set_cap(visible.saturating_mul(2).max(1));
        }

        let [tabs_area, search_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .areas(header_area);
        self.render_tabs(frame, tabs_area);
        self.render_search_box(frame, search_area);

        if self.grid().items.is_empty() {
            frame.render_widget(
                Paragraph::new(self.empty_message())
                    .alignment(Alignment::Center)
                    .style(Style::default().dim()),
                grid_area,
            );
        } else {
            self.request_previews();
            self.render_grid(frame, grid_area);
        }

        let (fallback_note, loaded, requested, failed) = match &self.mode {
            PreviewMode::Graphics { cache, .. } => {
                let (loaded, requested, failed) = preview::preview_stats(cache);
                ("", Some(loaded), Some(requested), Some(failed))
            }
            PreviewMode::Fallback => (
                " · no graphics protocol — title-only mode",
                None,
                None,
                None,
            ),
        };
        let loaded_note = loaded.map(|l| format!(" · {l} loaded")).unwrap_or_default();
        let requested_note = requested
            .filter(|r| *r > 0)
            .map(|r| format!(" · {r} loading"))
            .unwrap_or_default();
        let failed_note = failed
            .filter(|f| *f > 0)
            .map(|f| format!(" · {f} failed"))
            .unwrap_or_default();
        let page_note = self
            .grid()
            .pager
            .as_ref()
            .map(|pager| format!(" · {}", page_label(self.grid().page, pager.total())))
            .unwrap_or_default();
        let nav_line = format!(
            "{} items{page_note} · ←↑↓→ / hjkl move · u/d page · Enter/Space pick · q quit{fallback_note}{loaded_note}{requested_note}{failed_note}",
            self.grid().items.len()
        );
        let status_note = self
            .status
            .as_deref()
            .map(|s| format!(" — {s}"))
            .unwrap_or_default();
        let mode_note = if self.favs.is_local() {
            " · local favorites"
        } else {
            ""
        };
        let action_line =
            format!("Tab switch · c copy gif · y copy url · v favorite{mode_note}{status_note}");

        let [footer_nav, footer_actions] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .areas(footer_area);
        frame.render_widget(Paragraph::new(nav_line), footer_nav);
        frame.render_widget(Paragraph::new(action_line), footer_actions);
    }

    fn render_tabs(&self, frame: &mut ratatui::Frame, area: Rect) {
        let active = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().dim();
        let line = Line::from(vec![
            Span::styled(" gifdeck · ", dim),
            Span::styled(
                "[Search]",
                if self.tab == Tab::Search { active } else { dim },
            ),
            Span::styled(" ", dim),
            Span::styled(
                "[Favorites]",
                if self.tab == Tab::Favorites {
                    active
                } else {
                    dim
                },
            ),
            Span::styled(" · q quit", dim),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_search_box(&self, frame: &mut ratatui::Frame, area: Rect) {
        let line = if self.tab == Tab::Search && self.search_focused {
            Line::from(vec![
                Span::raw(" > "),
                Span::styled(self.query.clone(), Style::default().fg(Color::Yellow)),
                Span::styled(
                    "█",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        } else if self.tab == Tab::Search && self.query.is_empty() {
            Line::from(Span::styled(
                " > type a query… (/ focus, Enter search)",
                Style::default().dim(),
            ))
        } else {
            Line::from(vec![
                Span::styled(" > ", Style::default().dim()),
                Span::styled(self.query.clone(), Style::default().dim()),
            ])
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn empty_message(&self) -> String {
        match self.tab {
            Tab::Search if self.query.trim().is_empty() => {
                "type a query, then Enter — press / to focus the search box\nq quit".into()
            }
            Tab::Search => format!(
                "no results for '{}' — press / to edit, Enter to search\nq quit",
                self.query.trim()
            ),
            Tab::Favorites => {
                "no favorites yet — press v on the Search tab to star one\nq quit".into()
            }
        }
    }

    fn render_grid(&self, frame: &mut ratatui::Frame, area: Rect) {
        let cols = (self.cols as usize).max(1);
        let rows = (self.rows as usize).max(1);
        let cell_h = cell_height();
        for slot in 0..(cols * rows) {
            let idx = self.grid().first_item + slot;
            if idx >= self.grid().items.len() {
                break;
            }
            let c = slot % cols;
            let r = slot / cols;
            let x = area.x + (c as u16) * MIN_CELL_W;
            let y = area.y + (r as u16) * cell_h;
            let width = MIN_CELL_W.min(area.width.saturating_sub((c as u16) * MIN_CELL_W));
            let height = cell_h.min(area.height.saturating_sub((r as u16) * cell_h));
            if width == 0 || height == 0 {
                continue;
            }
            self.render_cell(frame, Rect::new(x, y, width, height), idx);
        }
    }

    fn render_cell(&self, frame: &mut ratatui::Frame, area: Rect, idx: usize) {
        let item = &self.grid().items[idx];
        let selected = idx == self.grid().selected;
        let accent = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let title = item.title.trim();
        let index_label = format!("{:04}", idx + 1);
        let star = if self.fav_ids.contains(&item.id) {
            "★"
        } else {
            "☆"
        };
        let headline = if title.is_empty() {
            format!("{star} {index_label}")
        } else {
            format!("{star} {index_label} · {title}")
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(accent)
            .title(headline);
        frame.render_widget(block.clone(), area);
        let inner = block.inner(area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        match &self.mode {
            PreviewMode::Graphics { cache, .. } => {
                let image_h = PREVIEW_SIZE.height.min(inner.height.saturating_sub(1));
                if image_h > 0 {
                    let [img_area, text_area] = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(image_h), Constraint::Min(1)])
                        .areas(inner);
                    if !item.preview_url.is_empty() {
                        preview::render_preview(frame, img_area, cache, &item.preview_url);
                    }
                    frame.render_widget(
                        Paragraph::new(truncate(title, text_area.width)).style(accent),
                        text_area,
                    );
                }
            }
            PreviewMode::Fallback => {
                frame.render_widget(
                    Paragraph::new(truncate(title, inner.width)).style(accent),
                    inner,
                );
            }
        }
    }
}

fn truncate(s: &str, width: u16) -> String {
    let w = width as usize;
    if s.chars().count() <= w {
        return s.to_string();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Run the unified picker. Returns the selected GIF URL, if any.
pub async fn run(mut app: App) -> Result<Option<String>> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("stdout is not a terminal; the TUI requires an interactive session");
    }

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    if io::stdin().is_terminal() {
        match Picker::from_query_stdio() {
            Ok(picker) if picker.protocol_type() != ProtocolType::Halfblocks => {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .unwrap_or_default();
                let cache = Arc::new(Mutex::new(PreviewCache::new(MAX_CACHE_CAP)));
                let loader = PreviewLoader::new(client, Arc::clone(&cache), picker);
                app.enable_previews(cache, loader);
            }
            _ => {}
        }
    }

    let result: Result<Option<String>> = async {
        let tick = Duration::from_millis(50);
        while !app.should_quit {
            match app.pending.take() {
                Some(Pending::Page(dir)) => {
                    app.status = Some("loading…".into());
                    terminal.draw(|f| app.draw(f))?;
                    app.load_page(dir).await;
                }
                Some(Pending::Search) => {
                    let q = app.query.trim().to_string();
                    app.status = Some(format!("searching '{q}'…"));
                    terminal.draw(|f| app.draw(f))?;
                    app.run_search().await;
                }
                Some(Pending::Toggle) => {
                    app.toggle_favorite().await;
                }
                Some(Pending::CopyGif) => {
                    app.status = Some("copying gif…".into());
                    terminal.draw(|f| app.draw(f))?;
                    app.copy_gif().await;
                }
                Some(Pending::CopyUrl) => {
                    app.copy_url();
                }
                Some(Pending::TabLoad) => {
                    app.load_tab().await;
                }
                None => {}
            }
            terminal.draw(|f| app.draw(f))?;
            if event::poll(tick)? {
                if let Event::Key(key) = event::read()? {
                    app.handle_key(key);
                }
            }
        }
        Ok(app.selected_url)
    }
    .await;

    disable_raw_mode()?;
    stdout.execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::favs::FavsClient;
    use crate::store::LocalStore;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::path::PathBuf;

    fn items(n: usize) -> Vec<UrlItem> {
        (0..n)
            .map(|i| UrlItem {
                id: format!("id{i}"),
                title: format!("gif {i}"),
                url: format!("https://gifdeck.test/{i}.gif"),
                preview_url: format!("https://gifdeck.test/{i}-preview.gif"),
                provider: Provider::Klipy,
            })
            .collect()
    }

    fn test_app(items: Vec<UrlItem>) -> App {
        // A local backend pointed at a per-test temp path; tests that
        // don't toggle never touch it, and toggle tests build their own.
        let mut app = App::new(
            Config::default(),
            reqwest::Client::new(),
            providers::Source::Auto,
            FavsBackend::Local(LocalStore::at(local_store_path())),
        );
        app.search.items = items;
        app
    }

    fn local_store_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "gifdeck-app-test-store-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn server_backend(cfg: &Config) -> FavsBackend {
        FavsBackend::Server(FavsClient::new(cfg).unwrap())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn cfg_for(base: &str) -> Config {
        Config {
            klipy_api_key: None,
            giphy_api_key: None,
            favorites_api: Some(base.to_string()),
            favorites_token: Some("sekret".to_string()),
        }
    }

    #[test]
    fn grid_columns_from_width() {
        assert_eq!(grid_cols(28), 2);
        assert_eq!(grid_cols(42), 3);
        assert_eq!(grid_cols(80), 5);
        assert_eq!(grid_cols(13), 1);
        assert_eq!(grid_cols(0), 1);
    }

    #[test]
    fn visible_rows_from_height() {
        assert_eq!(visible_rows(14), 2);
        assert_eq!(visible_rows(21), 3);
        assert_eq!(visible_rows(10), 1);
        assert_eq!(visible_rows(0), 1);
    }

    #[test]
    fn nav_left_right_wrap() {
        assert_eq!(nav_left(0, 5), 4);
        assert_eq!(nav_left(3, 5), 2);
        assert_eq!(nav_right(4, 5), 0);
        assert_eq!(nav_right(0, 5), 1);
        assert_eq!(nav_right(0, 1), 0);
        assert_eq!(nav_left(0, 0), 0);
    }

    #[test]
    fn nav_up_down_wrap() {
        let len = 10;
        let cols = 3;
        assert_eq!(nav_down(0, cols, len), 3);
        assert_eq!(nav_down(6, cols, len), 9);
        assert_eq!(nav_down(9, cols, len), 0);
        assert_eq!(nav_down(8, cols, len), 2);
        assert_eq!(nav_up(9, cols, len), 6);
        assert_eq!(nav_up(3, cols, len), 0);
        assert_eq!(nav_up(2, cols, len), 9);
        assert_eq!(nav_up(0, cols, len), 9);
        assert_eq!(nav_up(20, 3, 21), 17);
    }

    #[test]
    fn nearest_slots_orders_by_distance() {
        let slots = nearest_slots(0, 4, 2, 8, 5);
        assert_eq!(slots.len(), 8);
        assert_eq!(slots[0], 5);
        assert!(slots[1..4].contains(&1));
        assert!(slots[1..4].contains(&4));
        assert!(slots[1..4].contains(&6));
        let pos7 = slots.iter().position(|&x| x == 7).unwrap();
        let pos3 = slots.iter().position(|&x| x == 3).unwrap();
        let pos0 = slots.iter().position(|&x| x == 0).unwrap();
        assert!(pos0 < pos7 && pos7 < pos3);
    }

    #[test]
    fn nearest_slots_stops_at_partial_last_row() {
        let slots = nearest_slots(0, 4, 2, 6, 4);
        assert_eq!(slots.len(), 6);
        assert!(slots.iter().all(|&i| i < 6));
        assert_eq!(slots[0], 4);
    }

    #[test]
    fn nearest_slots_empty_items_is_safe() {
        assert!(nearest_slots(0, 4, 2, 0, 0).is_empty());
    }

    #[test]
    fn ensure_visible_scrolls_down() {
        let mut app = test_app(items(20));
        app.cols = 3;
        app.rows = 2;
        app.search.selected = 10;
        app.ensure_visible();
        assert_eq!(app.search.first_item, 6);
    }

    #[test]
    fn ensure_visible_scrolls_up() {
        let mut app = test_app(items(20));
        app.cols = 3;
        app.rows = 2;
        app.search.first_item = 6;
        app.search.selected = 5;
        app.ensure_visible();
        assert_eq!(app.search.first_item, 3);
    }

    #[test]
    fn ensure_visible_clamps_to_last_row() {
        let mut app = test_app(items(5));
        app.cols = 3;
        app.rows = 3;
        app.search.selected = 4;
        app.ensure_visible();
        assert_eq!(app.search.first_item, 0);
    }

    #[test]
    fn ensure_visible_empty_items_is_safe() {
        let mut app = test_app(Vec::new());
        app.cols = 3;
        app.rows = 2;
        app.search.selected = 0;
        app.ensure_visible();
        assert_eq!(app.search.first_item, 0);
    }

    #[test]
    fn gif_result_maps_to_url_item() {
        let gif = GifResult {
            id: "abc".into(),
            title: String::new(),
            url: "https://x/y.gif".into(),
            preview_url: "https://x/y-preview.gif".into(),
            provider: Provider::Klipy,
        };
        let item = UrlItem::from(gif);
        assert_eq!(item.id, "abc");
        assert_eq!(item.title, "abc");
        assert_eq!(item.url, "https://x/y.gif");
        assert_eq!(item.preview_url, "https://x/y-preview.gif");
        assert_eq!(item.provider, Provider::Klipy);
    }

    #[test]
    fn fav_item_maps_to_url_item() {
        let fav = FavItem {
            id: "f1".into(),
            url: "https://x/f1.gif".into(),
            preview: "https://x/f1-preview.gif".into(),
            provider: "giphy".into(),
            title: String::new(),
            use_count: 0,
            added_at: None,
            last_used: None,
        };
        let item = UrlItem::from(fav);
        assert_eq!(item.id, "f1");
        assert_eq!(item.title, "f1");
        assert_eq!(item.provider, Provider::Giphy);
        let back = item.to_gif_result();
        assert_eq!(back.id, "f1");
        assert_eq!(back.url, "https://x/f1.gif");
        assert_eq!(back.provider, Provider::Giphy);
    }

    #[test]
    fn provider_labels_round_trip() {
        assert_eq!(Provider::Giphy.label(), "giphy");
        assert_eq!(Provider::Klipy.label(), "klipy");
        assert_eq!(Provider::from_label("giphy"), Provider::Giphy);
        assert_eq!(Provider::from_label("GIPHY"), Provider::Giphy);
        assert_eq!(Provider::from_label("klipy"), Provider::Klipy);
        assert_eq!(Provider::from_label("weird"), Provider::Klipy);
    }

    #[test]
    fn truncate_respects_width() {
        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn space_selects_current_item() {
        let mut app = test_app(items(3));
        app.search.selected = 2;
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.should_quit);
        assert_eq!(
            app.selected_url.as_deref(),
            Some("https://gifdeck.test/2.gif")
        );
    }

    #[test]
    fn d_and_u_half_page_without_pager() {
        let mut down = test_app(items(40));
        down.cols = 4;
        down.rows = 6;
        down.search.selected = 0;
        down.handle_key(key(KeyCode::Char('d')));
        assert_eq!(down.search.selected, 12);

        let mut up = test_app(items(40));
        up.cols = 4;
        up.rows = 6;
        up.search.selected = 30;
        up.handle_key(key(KeyCode::Char('u')));
        assert_eq!(up.search.selected, 18);

        let mut c = test_app(items(40));
        c.cols = 4;
        c.rows = 6;
        c.search.selected = 0;
        c.handle_key(ctrl('d'));
        assert_eq!(c.search.selected, 12);
    }

    #[test]
    fn d_clamps_at_end() {
        let mut app = test_app(items(40));
        app.cols = 4;
        app.rows = 6;
        app.search.selected = 38;
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.search.selected, 39, "half-page clamps to last item");
    }

    fn search_pager(next: Option<providers::PageCursor>, total: Option<usize>) -> Pager {
        Pager::search(
            reqwest::Client::new(),
            Config::default(),
            providers::Source::Auto,
            "cats".to_string(),
            next,
            total,
        )
    }

    #[test]
    fn d_with_pager_queues_next_page() {
        let mut app = test_app(items(40));
        app.search.pager = Some(search_pager(
            Some(providers::PageCursor::Giphy { offset: 40 }),
            None,
        ));
        app.cols = 4;
        app.rows = 6;
        app.search.selected = 7;
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.pending, Some(Pending::Page(PageDir::Next)));
        assert_eq!(app.search.selected, 7, "cursor does not move when paging");

        app.pending = None;
        app.handle_key(ctrl('d'));
        assert_eq!(app.pending, Some(Pending::Page(PageDir::Next)));
    }

    #[test]
    fn u_with_pager_queues_prev_page() {
        let mut app = test_app(items(40));
        app.search.pager = Some(search_pager(
            Some(providers::PageCursor::Giphy { offset: 40 }),
            None,
        ));
        app.search.page = 1;
        app.search.selected = 7;
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(app.pending, Some(Pending::Page(PageDir::Prev)));
        assert_eq!(app.search.selected, 7, "cursor does not move when paging");

        app.pending = None;
        app.handle_key(ctrl('u'));
        assert_eq!(app.pending, Some(Pending::Page(PageDir::Prev)));
    }

    #[test]
    fn page_up_and_page_down_are_not_bound() {
        let mut app = test_app(items(40));
        app.search.pager = Some(search_pager(
            Some(providers::PageCursor::Giphy { offset: 40 }),
            Some(200),
        ));
        app.search.page = 1;
        app.search.selected = 7;
        for code in [KeyCode::PageDown, KeyCode::PageUp] {
            app.handle_key(key(code));
            assert_eq!(app.pending, None, "{code:?} must not page");
            assert_eq!(app.search.selected, 7, "{code:?} must not move the cursor");
            assert_eq!(app.search.page, 1, "{code:?} must not change the page");
            assert!(!app.should_quit, "{code:?} must not quit");
        }
    }

    #[tokio::test]
    async fn pager_next_without_cursor_reports_last_page() {
        let mut pager = search_pager(None, None);
        assert!(matches!(
            pager.turn(PageDir::Next, 0).await,
            Ok(PageTurn::AtLast)
        ));
    }

    #[tokio::test]
    async fn pager_prev_at_first_page_is_refused() {
        let mut pager = search_pager(
            Some(providers::PageCursor::Klipy {
                pos: "abc".to_string(),
            }),
            None,
        );
        assert!(matches!(
            pager.turn(PageDir::Prev, 0).await,
            Ok(PageTurn::AtFirst)
        ));

        // Local backend on page 0: refused before touching the store.
        let mut pager = Pager::favs(FavsBackend::Local(LocalStore::at(local_store_path())), None);
        assert!(matches!(
            pager.turn(PageDir::Prev, 0).await,
            Ok(PageTurn::AtFirst)
        ));
    }

    #[tokio::test]
    async fn pager_next_past_known_total_is_refused_offline() {
        // 100 items = 2 pages; on page 1 (0-based) a Next turn must not
        // hit the backing store at all — the path points at a file that
        // does not exist, proving the refusal was local.
        let store = LocalStore::at(std::env::temp_dir().join(format!(
            "gifdeck-pager-refusal-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        )));
        let mut pager = Pager::favs(FavsBackend::Local(store), Some(100));
        assert!(matches!(
            pager.turn(PageDir::Next, 1).await,
            Ok(PageTurn::AtLast)
        ));

        // 101 items = 3 pages; page 1 still has a successor, so the turn
        // proceeds and fetches: with a seeded store it returns a real page,
        // proving the shortcut did not trigger.
        let seeded: Vec<crate::favs::FavItem> = (0..101)
            .map(|i| crate::favs::FavItem {
                id: format!("f{i}"),
                url: format!("https://x/{i}.gif"),
                preview: String::new(),
                provider: "klipy".into(),
                title: String::new(),
                use_count: 0,
                added_at: None,
                last_used: None,
            })
            .collect();
        let store = LocalStore::at(std::env::temp_dir().join(format!(
            "gifdeck-pager-refusal-{}-{:?}-b.json",
            std::process::id(),
            std::thread::current().id()
        )));
        store.save_all(&seeded).unwrap();
        let mut pager = Pager::favs(FavsBackend::Local(store.clone()), Some(101));
        let result = pager.turn(PageDir::Next, 1).await;
        assert!(
            matches!(&result, Ok(PageTurn::Page(items)) if items.len() == 1
                && items[0].id == "f100"),
            "page 2 of 3 (offset 100) must be fetched, got {result:?}"
        );
        let _ = std::fs::remove_file(store.path());

        // Same refusal logic on the search pager, purely from the total.
        let mut pager = search_pager(Some(providers::PageCursor::Giphy { offset: 50 }), Some(100));
        assert!(matches!(
            pager.turn(PageDir::Next, 1).await,
            Ok(PageTurn::AtLast)
        ));
    }

    #[test]
    fn apply_page_replaces_items_and_resets_cursor() {
        let mut app = test_app(items(40));
        app.cols = 4;
        app.rows = 6;
        app.search.selected = 17;
        app.search.first_item = 16;
        app.search.page = 2;
        app.apply_page(items(3));
        assert_eq!(app.search.items.len(), 3);
        assert_eq!(app.search.items[0].url, "https://gifdeck.test/0.gif");
        assert_eq!(app.search.selected, 0);
        assert_eq!(app.search.first_item, 0);
        assert_eq!(app.status, None);
    }

    #[test]
    fn tab_key_switches_tab_and_swaps_grid() {
        let mut app = test_app(items(3));
        app.search.pager = Some(search_pager(None, Some(3)));
        app.favorites.items = items(5);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Favorites);
        assert_eq!(app.pending, Some(Pending::TabLoad));
        assert_eq!(app.grid().items.len(), 5);
        assert!(app.grid().pager.is_none(), "tabs keep separate pagers");
        assert!(!app.search_focused);

        app.pending = None;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Search);
        assert_eq!(app.pending, Some(Pending::TabLoad));
        assert_eq!(app.grid().items.len(), 3);
        assert!(app.grid().pager.is_some());
    }

    #[test]
    fn switch_tab_preserves_per_tab_cursors() {
        let mut app = test_app(items(3));
        app.favorites.items = items(9);
        app.search.selected = 1;
        app.favorites.selected = 7;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.favorites.selected, 7);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.search.selected, 1);
    }

    #[test]
    fn tab_from_search_box_blurs_and_switches() {
        let mut app = test_app(items(3));
        app.search_focused = true;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Favorites);
        assert!(!app.search_focused);
        assert_eq!(app.pending, Some(Pending::TabLoad));
    }

    #[test]
    fn slash_focuses_search_box() {
        let mut app = test_app(items(3));
        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.search_focused);
    }

    #[test]
    fn typing_while_focused_builds_query_without_commands() {
        let mut app = test_app(items(3));
        app.search_focused = true;
        for c in "cat".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.query, "cat");
        // Keys that are commands when blurred (j, v, c, y, d, u, q) are
        // plain input while focused, and never queue actions or move.
        for c in "jvcyduq".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.query, "catjvcyduq");
        assert_eq!(app.pending, None);
        assert_eq!(app.search.selected, 0);
        assert!(!app.should_quit);
    }

    #[test]
    fn backspace_and_ctrl_u_edit_query() {
        let mut app = test_app(items(3));
        app.search_focused = true;
        for c in "cat".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.query, "ca");
        app.handle_key(ctrl('u'));
        assert_eq!(app.query, "");
    }

    #[test]
    fn esc_clears_then_blurs_but_never_eats_quit() {
        let mut app = test_app(items(3));
        app.search_focused = true;
        for c in "cat".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.query, "", "first Esc clears the query");
        assert!(app.search_focused);
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.search_focused, "second Esc blurs");
        assert!(!app.should_quit, "blur Esc must not quit");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.should_quit, "Esc while blurred quits");
    }

    #[test]
    fn enter_queues_search_and_blurs() {
        let mut app = test_app(items(3));
        app.search_focused = true;
        for c in "cat".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.search_focused);
        assert_eq!(app.pending, Some(Pending::Search));

        // Empty query: Enter blurs but does not hammer the API.
        app.pending = None;
        app.search_focused = true;
        app.query.clear();
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.search_focused);
        assert_eq!(app.pending, None);
        assert!(app.status.as_deref().unwrap().contains("type a query"));
    }

    #[test]
    fn ctrl_c_quits_from_search_box() {
        let mut app = test_app(items(3));
        app.search_focused = true;
        app.handle_key(ctrl('c'));
        assert!(app.should_quit);
    }

    #[test]
    fn v_c_y_without_selection_are_noops() {
        let mut app = test_app(Vec::new());
        for c in ['v', 'c', 'y'] {
            app.handle_key(key(KeyCode::Char(c)));
            assert_eq!(app.pending, None, "'{c}' with no items must not queue");
        }
    }

    #[test]
    fn v_c_y_with_selection_queue_actions() {
        let mut app = test_app(items(2));
        app.handle_key(key(KeyCode::Char('v')));
        assert_eq!(app.pending, Some(Pending::Toggle));
        app.pending = None;
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.pending, Some(Pending::CopyGif));
        app.pending = None;
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.pending, Some(Pending::CopyUrl));
    }

    #[tokio::test]
    async fn toggle_saves_then_removes_favorite() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/favorites"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "id0",
                "url": "https://gifdeck.test/0.gif",
                "preview": "https://gifdeck.test/0-preview.gif",
                "provider": "klipy",
                "title": "gif 0"
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/favorites/id0"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let cfg = cfg_for(&server.uri());
        let mut app = App::new(
            cfg.clone(),
            reqwest::Client::new(),
            providers::Source::Auto,
            server_backend(&cfg),
        );
        app.search.items = items(1);

        assert!(!app.fav_ids.contains("id0"));
        app.toggle_favorite().await;
        assert!(app.fav_ids.contains("id0"));
        assert!(app.status.as_deref().unwrap().contains("saved to favorites"));

        app.toggle_favorite().await;
        assert!(!app.fav_ids.contains("id0"));
        assert!(app.status.as_deref().unwrap().contains("removed from favorites"));
    }

    #[tokio::test]
    async fn toggle_server_error_surfaces_in_status() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/favorites"))
            .respond_with(
                wiremock::ResponseTemplate::new(502)
                    .set_body_json(serde_json::json!({"error": "upstream down"})),
            )
            .mount(&server)
            .await;

        let cfg = cfg_for(&server.uri());
        let mut app = App::new(
            cfg.clone(),
            reqwest::Client::new(),
            providers::Source::Auto,
            server_backend(&cfg),
        );
        app.search.items = items(1);

        app.toggle_favorite().await;
        assert!(app.fav_ids.is_empty(), "failed save must not mark as fav");
        let status = app.status.as_deref().unwrap();
        assert!(status.contains("502"), "got: {status}");
    }

    #[tokio::test]
    async fn toggle_local_backend_writes_store() {
        let path = local_store_path();
        let mut app = App::new(
            Config::default(),
            reqwest::Client::new(),
            providers::Source::Auto,
            FavsBackend::Local(LocalStore::at(&path)),
        );
        app.search.items = items(1);

        app.toggle_favorite().await;
        assert!(app.fav_ids.contains("id0"));
        assert!(app
            .status
            .as_deref()
            .unwrap()
            .contains("saved to local favorites"));
        let stored = LocalStore::at(&path).load().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, "id0");
        assert_eq!(stored[0].url, "https://gifdeck.test/0.gif");

        app.toggle_favorite().await;
        assert!(!app.fav_ids.contains("id0"));
        assert!(LocalStore::at(&path).load().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn toggle_local_backend_surfaces_damaged_store() {
        let path = local_store_path();
        std::fs::write(&path, "{ broken").unwrap();
        let mut app = App::new(
            Config::default(),
            reqwest::Client::new(),
            providers::Source::Auto,
            FavsBackend::Local(LocalStore::at(&path)),
        );
        app.search.items = items(1);

        app.toggle_favorite().await;
        assert!(app.fav_ids.is_empty(), "a damaged store must not be toggled");
        let status = app.status.as_deref().unwrap();
        assert!(status.contains("favorite failed"), "got: {status}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn refresh_favorites_keeps_cursor_in_sync() {
        let server = wiremock::MockServer::start().await;
        // Page shrinks from 3 items to 2 after a delete.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/favorites"))
            .and(wiremock::matchers::query_param("limit", "50"))
            .and(wiremock::matchers::query_param("offset", "0"))
            .respond_with(wiremock::ResponseTemplate::new(200)
                .insert_header("X-Total-Count", "2")
                .set_body_json(serde_json::json!([
                    { "id": "a", "url": "https://x/a.gif", "preview": "", "provider": "klipy", "title": "A" },
                    { "id": "b", "url": "https://x/b.gif", "preview": "", "provider": "klipy", "title": "B" }
                ])))
            .mount(&server)
            .await;

        let cfg = cfg_for(&server.uri());
        let backend = server_backend(&cfg);
        let mut app = App::new(
            cfg,
            reqwest::Client::new(),
            providers::Source::Auto,
            backend.clone(),
        );
        app.favorites.items = items(3);
        app.favorites.selected = 1;
        app.favorites.page = 0;
        app.favorites.pager = Some(Pager::favs(backend, Some(3)));

        app.refresh_favorites_page().await;
        assert_eq!(app.favorites.items.len(), 2);
        assert_eq!(app.favorites.selected, 1, "cursor kept when in range");
        assert!(matches!(
            &app.favorites.pager,
            Some(Pager::Favs { total: Some(2), .. })
        ));
    }

    #[tokio::test]
    async fn refresh_favorites_steps_back_when_page_empties() {
        let server = wiremock::MockServer::start().await;
        // Current page (offset 50) is now empty; page 0 has one item.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/favorites"))
            .and(wiremock::matchers::query_param("offset", "50"))
            .respond_with(wiremock::ResponseTemplate::new(200)
                .insert_header("X-Total-Count", "1")
                .set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/favorites"))
            .and(wiremock::matchers::query_param("offset", "0"))
            .respond_with(wiremock::ResponseTemplate::new(200)
                .insert_header("X-Total-Count", "1")
                .set_body_json(serde_json::json!([
                    { "id": "a", "url": "https://x/a.gif", "preview": "", "provider": "klipy", "title": "A" }
                ])))
            .mount(&server)
            .await;

        let cfg = cfg_for(&server.uri());
        let backend = server_backend(&cfg);
        let mut app = App::new(
            cfg,
            reqwest::Client::new(),
            providers::Source::Auto,
            backend.clone(),
        );
        app.favorites.items = items(1);
        app.favorites.page = 1;
        app.favorites.pager = Some(Pager::favs(backend, Some(51)));

        app.refresh_favorites_page().await;
        assert_eq!(app.favorites.page, 0, "emptied page steps back to page 0");
        assert_eq!(app.favorites.items.len(), 1);
    }

    #[test]
    fn page_label_formats_pages() {
        assert_eq!(page_label(0, Some(50)), "page 1/1");
        assert_eq!(page_label(1, Some(100)), "page 2/2");
        assert_eq!(page_label(2, Some(101)), "page 3/3");
        assert_eq!(page_label(4, Some(24310)), "page 5/487");
        assert_eq!(page_label(2, None), "page 3");
        assert_eq!(page_label(0, Some(0)), "page 1");
    }
}