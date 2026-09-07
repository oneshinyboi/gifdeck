use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

/// Minimum terminal width the list UI is designed for.
const MIN_WIDTH: u16 = 40;

/// A selectable row: what's shown vs. what gets pasted when selected.
#[derive(Debug, Clone)]
pub struct UrlItem {
    pub title: String,
    pub url: String,
}

/// TUI application state (list only for session 1).
#[derive(Debug)]
pub struct App {
    items: Vec<UrlItem>,
    list_state: ListState,
    should_quit: bool,
    selected_url: Option<String>,
    heading: String,
}

impl App {
    pub fn new(items: Vec<UrlItem>, heading: impl Into<String>) -> Self {
        let mut list_state = ListState::default();
        if !items.is_empty() {
            list_state.select(Some(0));
        }
        App {
            items,
            list_state,
            should_quit: false,
            selected_url: None,
            heading: heading.into(),
        }
    }

    fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    fn move_down(&mut self) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        let next = match self.selected() {
            None => 0,
            Some(i) => (i + 1).min(n - 1),
        };
        self.list_state.select(Some(next));
    }

    fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let cur = self.selected().unwrap_or(0);
        self.list_state.select(Some(cur.saturating_sub(1)));
    }

    /// Select the highlighted item, remembering its URL.
    fn choose(&mut self) {
        if let Some(i) = self.selected() {
            self.selected_url = Some(self.items[i].url.clone());
            self.should_quit = true;
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(),
            KeyCode::Enter | KeyCode::Char('\n') => self.choose(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            _ => {}
        }
    }

    /// Draw the list onto the provided frame.
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let too_small = area.width < MIN_WIDTH;

        let [_, list_area, footer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(1),
                ]
                .as_ref(),
            )
            .areas(area);

        let items: Vec<ListItem> = self
            .items
            .iter()
            .map(|it| ListItem::new(it.title.as_str()))
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", self.heading)),
            )
            .highlight_style(ratatui::style::Style::default().reversed());
        frame.render_stateful_widget(list, list_area, &mut self.list_state);

        let footer = if too_small {
            format!(
                "terminal too small ({} cols; {} needed)",
                area.width, MIN_WIDTH
            )
        } else {
            format!(
                "{} items · ↑/k ↓/j scroll · Enter copy · q quit",
                self.items.len()
            )
        };
        frame.render_widget(Paragraph::new(footer), footer_area);
    }
}

/// Simple polling event loop over crossterm events.
pub struct EventLoop;

impl EventLoop {
    pub fn next_event(&self) -> io::Result<Event> {
        event::poll(Duration::from_millis(250))?;
        event::read()
    }
}

/// Run the interactive list TUI. Returns the selected URL, if any.
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
    let loop_ = EventLoop;

    let result = (|| -> Result<Option<String>> {
        while !app.should_quit {
            terminal.draw(|f| app.draw(f))?;
            if let Event::Key(key) = loop_.next_event()? {
                app.handle_key(key);
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