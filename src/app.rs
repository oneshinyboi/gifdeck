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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use ratatui_image::picker::{Picker, ProtocolType};

use crate::preview::{self, PreviewCache, PreviewLoader, MAX_CACHE_CAP, PREVIEW_SIZE};
use crate::providers;

/// A selectable GIF: what's shown vs. what gets pasted when selected.
#[derive(Debug, Clone)]
pub struct UrlItem {
    pub title: String,
    pub url: String,
    pub preview_url: String,
}

impl From<providers::GifResult> for UrlItem {
    fn from(r: providers::GifResult) -> Self {
        let title = if r.title.is_empty() { r.id } else { r.title };
        UrlItem {
            title,
            url: r.url,
            preview_url: r.preview_url,
        }
    }
}

/// Minimum width of one grid cell (PREVIEW_WIDTH + 2 border columns).
pub const MIN_CELL_W: u16 = PREVIEW_SIZE.width + 2;
/// Extra rows per cell besides the image: 2 borders + 1 title line.
const CELL_EXTRA_H: u16 = 3;

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
    items: Vec<UrlItem>,
    heading: String,
    selected: usize,
    first_item: usize,
    cols: u16,
    rows: u16,
    should_quit: bool,
    selected_url: Option<String>,
    mode: PreviewMode,
}

impl App {
    pub fn new(items: Vec<UrlItem>, heading: impl Into<String>) -> Self {
        App {
            items,
            heading: heading.into(),
            selected: 0,
            first_item: 0,
            cols: 1,
            rows: 1,
            should_quit: false,
            selected_url: None,
            mode: PreviewMode::Fallback,
        }
    }

    pub fn enable_previews(
        &mut self,
        cache: Arc<Mutex<PreviewCache>>,
        loader: PreviewLoader,
    ) {
        self.mode = PreviewMode::Graphics { cache, loader };
    }

    /// Keep `selected` inside the visible window while staying row-aligned.
    fn ensure_visible(&mut self) {
        let len = self.items.len();
        if len == 0 {
            return;
        }
        let cols = (self.cols as usize).max(1);
        let rows = (self.rows as usize).max(1);
        let visible = rows.saturating_mul(cols);

        if self.selected < self.first_item {
            self.first_item = (self.selected / cols) * cols;
        } else if self.selected >= self.first_item + visible {
            let sel_row = self.selected / cols;
            let new_first_row = sel_row.saturating_sub(rows.saturating_sub(1));
            self.first_item = new_first_row * cols;
        }

        let max_first = (len.saturating_sub(1) / cols) * cols;
        if self.first_item > max_first {
            self.first_item = max_first;
        }
    }

    fn choose(&mut self) {
        if let Some(item) = self.items.get(self.selected) {
            self.selected_url = Some(item.url.clone());
            self.should_quit = true;
        }
    }

    fn step(&mut self, f: impl Fn(usize) -> usize) {
        self.selected = f(self.selected);
        self.ensure_visible();
    }

    fn scroll_down(&mut self, rows: usize) {
        let len = self.items.len();
        if len == 0 {
            return;
        }
        let jump = rows
            .max(1)
            .saturating_mul(self.cols.max(1) as usize);
        self.selected = if self.selected + jump >= len {
            len - 1
        } else {
            self.selected + jump
        };
        self.ensure_visible();
    }

    fn scroll_up(&mut self, rows: usize) {
        let len = self.items.len();
        if len == 0 {
            return;
        }
        let jump = rows
            .max(1)
            .saturating_mul(self.cols.max(1) as usize);
        self.selected = if self.selected < jump {
            0
        } else {
            self.selected - jump
        };
        self.ensure_visible();
    }

    fn page_down(&mut self) {
        self.scroll_down(self.rows.max(1) as usize);
    }

    fn page_up(&mut self) {
        self.scroll_up(self.rows.max(1) as usize);
    }

    fn half_page_down(&mut self) {
        self.scroll_down(((self.rows.max(1) / 2) as usize).max(1));
    }

    fn half_page_up(&mut self) {
        self.scroll_up(((self.rows.max(1) / 2) as usize).max(1));
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let len = self.items.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
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
            KeyCode::PageDown => self.page_down(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.half_page_down()
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.half_page_up()
            }
            KeyCode::Home => {
                self.selected = 0;
                self.ensure_visible();
            }
            KeyCode::End => {
                self.selected = len.saturating_sub(1);
                self.ensure_visible();
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
            self.first_item,
            cols,
            rows,
            self.items.len(),
            self.selected,
        ) {
            let url = &self.items[idx].preview_url;
            if !url.is_empty() {
                preview::ensure_requested(cache, loader, url);
            }
        }
    }

    /// Draw the grid onto the provided frame.
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let [header_area, grid_area, footer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(area);

        self.cols = grid_cols(grid_area.width);
        self.rows = visible_rows(grid_area.height);
        self.ensure_visible();

        if let PreviewMode::Graphics { cache, .. } = &self.mode {
            let visible = (self.cols as usize).saturating_mul(self.rows as usize);
            cache.lock().unwrap().set_cap(visible);
        }

        frame.render_widget(
            Paragraph::new(format!(" {} ", self.heading)),
            header_area,
        );

        if self.items.is_empty() {
            let msg = format!("{} — no items\npress q to quit", self.heading);
            frame.render_widget(
                Paragraph::new(msg)
                    .alignment(ratatui::layout::Alignment::Center)
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
            PreviewMode::Fallback => (" · no graphics protocol — title-only mode", None, None, None),
        };
        let loaded_note = loaded
            .map(|l| format!(" · {l} loaded"))
            .unwrap_or_default();
        let requested_note = requested
            .filter(|r| *r > 0)
            .map(|r| format!(" · {r} loading"))
            .unwrap_or_default();
        let failed_note = failed
            .filter(|f| *f > 0)
            .map(|f| format!(" · {f} failed"))
            .unwrap_or_default();
        let footer = format!(
            "{} items · ←↑↓→ / hjkl move · PgUp PgDn / ^U ^D page · Enter/Space pick · q quit{fallback_note}{loaded_note}{requested_note}{failed_note}",
            self.items.len()
        );
        frame.render_widget(Paragraph::new(footer), footer_area);
    }

    fn render_grid(&self, frame: &mut ratatui::Frame, area: Rect) {
        let cols = (self.cols as usize).max(1);
        let rows = (self.rows as usize).max(1);
        let cell_h = cell_height();
        for slot in 0..(cols * rows) {
            let idx = self.first_item + slot;
            if idx >= self.items.len() {
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
        let item = &self.items[idx];
        let selected = idx == self.selected;
        let accent = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let title = item.title.trim();
        let index_label = format!("{:04}", idx + 1);
        let headline = if title.is_empty() {
            index_label
        } else {
            format!("{} · {}", index_label, title)
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

/// Run the interactive GIF-grid TUI. Returns the selected GIF URL, if any.
pub fn run(items: Vec<UrlItem>, heading: &str) -> Result<Option<String>> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("stdout is not a terminal; the TUI requires an interactive session");
    }

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(items, heading);

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

    let result = (|| -> Result<Option<String>> {
        let tick = Duration::from_millis(50);
        while !app.should_quit {
            terminal.draw(|f| app.draw(f))?;
            if event::poll(tick)? {
                if let Event::Key(key) = event::read()? {
                    app.handle_key(key);
                }
            }
        }
        Ok(app.selected_url)
    })();

    disable_raw_mode()?;
    stdout.execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Copy a URL into the clipboard if a supported tool is available.
/// Silently does nothing when none is installed (best-effort, optional).
pub fn copy_to_clipboard(url: &str) {
    let wl = std::process::Command::new("wl-copy")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if wl.map(|s| s.success()).unwrap_or(false) {
        return;
    }
    let _ = std::process::Command::new("xclip")
        .arg("-selection")
        .arg("clipboard")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{GifResult, Provider};

    fn items(n: usize) -> Vec<UrlItem> {
        (0..n)
            .map(|i| UrlItem {
                title: format!("gif {i}"),
                url: format!("https://gifdeck.test/{i}.gif"),
                preview_url: format!("https://gifdeck.test/{i}-preview.gif"),
            })
            .collect()
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
        // 4x2 window starting at 0, cursor at index 5 (row 1, col 1).
        let slots = nearest_slots(0, 4, 2, 8, 5);
        assert_eq!(slots.len(), 8);
        // Distance tiers from cursor (1,1): 0 -> {5}, 1 -> {1,4,6}, 2 -> {0,2},
        // 4 -> {7}, 5 -> {3}. Assert the far corners sort last.
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
        // 4 columns, 2 rows requested, but only 6 items exist.
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
        let mut app = App::new(items(20), "t");
        app.cols = 3;
        app.rows = 2;
        app.selected = 10;
        app.ensure_visible();
        assert_eq!(app.first_item, 6);
    }

    #[test]
    fn ensure_visible_scrolls_up() {
        let mut app = App::new(items(20), "t");
        app.cols = 3;
        app.rows = 2;
        app.first_item = 6;
        app.selected = 5;
        app.ensure_visible();
        assert_eq!(app.first_item, 3);
    }

    #[test]
    fn ensure_visible_clamps_to_last_row() {
        let mut app = App::new(items(5), "t");
        app.cols = 3;
        app.rows = 3;
        app.selected = 4;
        app.ensure_visible();
        assert_eq!(app.first_item, 0);
    }

    #[test]
    fn ensure_visible_empty_items_is_safe() {
        let mut app = App::new(Vec::new(), "t");
        app.cols = 3;
        app.rows = 2;
        app.selected = 0;
        app.ensure_visible();
        assert_eq!(app.first_item, 0);
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
        assert_eq!(item.title, "abc");
        assert_eq!(item.url, "https://x/y.gif");
        assert_eq!(item.preview_url, "https://x/y-preview.gif");
    }

    #[test]
    fn truncate_respects_width() {
        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn space_selects_current_item() {
        let mut app = App::new(items(3), "t");
        app.selected = 2;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(app.should_quit);
        assert_eq!(app.selected_url.as_deref(), Some("https://gifdeck.test/2.gif"));
    }

    #[test]
    fn ctrl_d_and_ctrl_u_half_page() {
        let mut down = App::new(items(40), "t");
        down.cols = 4;
        down.rows = 6;
        down.selected = 0;
        // 6 rows -> half-page is 3 rows -> 12 cells.
        down.handle_key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(down.selected, 12);

        let mut up = App::new(items(40), "t");
        up.cols = 4;
        up.rows = 6;
        up.selected = 30;
        up.handle_key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(up.selected, 18);

        // Plain d/u must not page (reserved for future bindings).
        let mut plain = App::new(items(40), "t");
        plain.cols = 4;
        plain.rows = 6;
        plain.selected = 0;
        plain.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(plain.selected, 0, "plain 'd' does nothing");
    }

    #[test]
    fn ctrl_d_clamps_at_end() {
        let mut app = App::new(items(40), "t");
        app.cols = 4;
        app.rows = 6;
        app.selected = 38;
        app.handle_key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.selected, 39, "half-page clamps to last item");
    }
}