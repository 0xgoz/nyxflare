use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};

const LEGACY_CONFIG_PATH: &str = "config/accounts.json";
const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

fn main() -> Result<()> {
    let (config_path, config) = load_config()?;
    let accounts = config.accounts;

    let backend = if env::var("CF_TUI_OFFLINE").is_ok() {
        Backend::Mock(MockBackend::new())
    } else {
        Backend::Cloudflare(CloudflareBackend::new()?)
    };

    let mut app = App::new(config_path, accounts, backend)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    queue!(stdout, EnterAlternateScreen)?;
    stdout.flush()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        eprintln!("Application error: {err:?}");
    }

    Ok(())
}

fn load_config() -> Result<(PathBuf, Config)> {
    let config_path = default_config_path();

    let config = if config_path.exists() {
        Config::load(&config_path)?
    } else {
        let legacy_path = PathBuf::from(LEGACY_CONFIG_PATH);
        if legacy_path.exists() {
            Config::load(&legacy_path)?
        } else {
            Config::default()
        }
    };

    Ok((config_path, config))
}

fn default_config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nyxflare")
            .join("accounts.json")
    }

    #[cfg(not(target_os = "windows"))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from(".").join(".config"))
            .join("nyxflare")
            .join("accounts.json")
    }
}

fn run_app<B: DnsBackend>(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App<B>,
) -> Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && handle_key(key.code, app)?
        {
            return Ok(());
        }
    }
}

fn handle_key<B: DnsBackend>(code: KeyCode, app: &mut App<B>) -> Result<bool> {
    match app.mode {
        Mode::Normal => handle_normal_key(code, app),
        Mode::AddingAccount(_) => handle_add_account_key(code, app),
        Mode::RecordForm(_) => handle_record_form_key(code, app),
        Mode::ConfirmDelete(_) => handle_confirm_delete_key(code, app),
        Mode::Searching(_) => handle_search_key(code, app),
    }
}

fn handle_normal_key<B: DnsBackend>(code: KeyCode, app: &mut App<B>) -> Result<bool> {
    match code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('r') => {
            app.refresh_current()?;
        }
        KeyCode::Char('a') => {
            app.start_add_account();
        }
        KeyCode::Char('/') => {
            app.mode = Mode::Searching(app.record_filter.clone());
        }
        KeyCode::Char('n') => {
            app.start_record_form(false);
        }
        KeyCode::Char('e') => {
            app.start_record_form(true);
        }
        KeyCode::Char('d') => {
            app.ask_delete_record();
        }
        KeyCode::BackTab => {
            app.focus = match app.focus {
                Focus::Accounts => Focus::Records,
                Focus::Zones => Focus::Accounts,
                Focus::Records => Focus::Zones,
            }
        }
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::Accounts => Focus::Zones,
                Focus::Zones => Focus::Records,
                Focus::Records => Focus::Accounts,
            }
        }
        KeyCode::Up => match app.focus {
            Focus::Accounts => app.previous_account()?,
            Focus::Zones => app.previous_zone()?,
            Focus::Records => app.previous_record(),
        },
        KeyCode::Down => match app.focus {
            Focus::Accounts => app.next_account()?,
            Focus::Zones => app.next_zone()?,
            Focus::Records => app.next_record(),
        },
        KeyCode::PageDown => app.next_page(),
        KeyCode::PageUp => app.previous_page(),
        _ => {}
    }

    Ok(false)
}

fn handle_add_account_key<B: DnsBackend>(code: KeyCode, app: &mut App<B>) -> Result<bool> {
    let Some(form) = (match &mut app.mode {
        Mode::AddingAccount(form) => Some(form),
        _ => None,
    }) else {
        return Ok(false);
    };

    match code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.ensure_onboarding_prompt();
        }
        KeyCode::Enter => {
            if form.field_index < 2 {
                form.next_field();
            } else {
                match form.build_account() {
                    Ok(account) => {
                        app.finish_add_account(account)?;
                    }
                    Err(msg) => {
                        app.last_message = msg.to_string();
                    }
                }
            }
        }
        KeyCode::Tab | KeyCode::Down => form.next_field(),
        KeyCode::BackTab | KeyCode::Up => form.previous_field(),
        KeyCode::Backspace => form.backspace(),
        KeyCode::Char(c) => form.insert_char(c),
        _ => {}
    }

    Ok(false)
}

fn handle_record_form_key<B: DnsBackend>(code: KeyCode, app: &mut App<B>) -> Result<bool> {
    let Some(form) = (match &mut app.mode {
        Mode::RecordForm(form) => Some(form),
        _ => None,
    }) else {
        return Ok(false);
    };

    match code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
        }
        KeyCode::Tab | KeyCode::Down => form.field_index = (form.field_index + 1).min(4),
        KeyCode::BackTab | KeyCode::Up => {
            if form.field_index > 0 {
                form.field_index -= 1;
            }
        }
        KeyCode::Char(' ') if form.field_index == 4 => {
            form.draft.proxied = !form.draft.proxied;
        }
        KeyCode::Enter => {
            if form.field_index < 4 {
                form.field_index += 1;
            } else {
                let record_id = form.target_id.clone().unwrap_or_else(|| "new".to_string());
                match form.draft.to_record(record_id.clone()) {
                    Ok(record) => {
                        if form.is_edit {
                            app.update_record(record.clone())?;
                        } else {
                            app.create_record(record)?;
                        }
                    }
                    Err(err) => app.last_message = err.to_string(),
                }
            }
        }
        KeyCode::Backspace => match form.field_index {
            0 => {
                form.draft.name.pop();
            }
            1 => {
                form.draft.record_type.pop();
            }
            2 => {
                form.draft.content.pop();
            }
            3 => {
                form.draft.ttl.pop();
            }
            _ => {}
        },
        KeyCode::Char(c) => match form.field_index {
            0 => form.draft.name.push(c),
            1 => form.draft.record_type.push(c),
            2 => form.draft.content.push(c),
            3 => form.draft.ttl.push(c),
            _ => {}
        },
        _ => {}
    }

    Ok(false)
}

fn handle_confirm_delete_key<B: DnsBackend>(code: KeyCode, app: &mut App<B>) -> Result<bool> {
    let Some(confirm) = (match &app.mode {
        Mode::ConfirmDelete(c) => Some(c.clone()),
        _ => None,
    }) else {
        return Ok(false);
    };

    match code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Enter => {
            app.delete_record(confirm.record_id)?;
            app.mode = Mode::Normal;
        }
        _ => {}
    }

    Ok(false)
}

fn handle_search_key<B: DnsBackend>(code: KeyCode, app: &mut App<B>) -> Result<bool> {
    let Some(current) = (match &mut app.mode {
        Mode::Searching(text) => Some(text),
        _ => None,
    }) else {
        return Ok(false);
    };

    match code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
        }
        KeyCode::Enter => {
            app.record_filter = current.clone();
            app.record_page = 0;
            app.selected_record = 0;
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            current.pop();
        }
        KeyCode::Char(c) => current.push(c),
        _ => {}
    }

    Ok(false)
}

fn draw<B: DnsBackend>(frame: &mut Frame<'_>, app: &mut App<B>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(4)])
        .split(frame.size());

    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
        .split(chunks[0]);

    draw_accounts(frame, body_chunks[0], app);
    draw_zones_and_records(frame, body_chunks[1], app);
    draw_status(frame, chunks[1], app);

    match &app.mode {
        Mode::AddingAccount(form) => draw_account_form(frame, form),
        Mode::RecordForm(form) => draw_record_form(frame, form),
        Mode::ConfirmDelete(confirm) => draw_confirm_delete(frame, confirm),
        Mode::Searching(text) => draw_search_overlay(frame, text),
        Mode::Normal => {}
    }
}

fn draw_accounts<B: DnsBackend>(frame: &mut Frame<'_>, area: ratatui::prelude::Rect, app: &App<B>) {
    let items: Vec<ListItem> = app
        .accounts
        .iter()
        .map(|account| {
            let mut spans = vec![Span::raw(account.name.clone())];
            if let Some(id) = &account.account_id {
                spans.push(Span::raw(format!("  ({id})")));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default();
    if !app.accounts.is_empty() {
        state.select(Some(app.selected_account));
    }

    let title = match app.focus {
        Focus::Accounts => "Accounts (selected)",
        Focus::Zones | Focus::Records => "Accounts",
    };

    let accounts_list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(match app.focus {
                    Focus::Accounts => Style::default().fg(Color::Cyan),
                    _ => Style::default(),
                }),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol("→ ");

    frame.render_stateful_widget(accounts_list, area, &mut state);
}

fn draw_zones_and_records<B: DnsBackend>(
    frame: &mut Frame<'_>,
    area: ratatui::prelude::Rect,
    app: &mut App<B>,
) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    app.update_record_page_size(vertical[1].height);

    let zones_title = match app.focus {
        Focus::Zones => "Zones (selected)",
        _ => "Zones",
    };

    let zone_items: Vec<ListItem> = app
        .zones
        .iter()
        .map(|zone| ListItem::new(zone.name.clone()))
        .collect();

    let mut zone_state = ListState::default();
    if !app.zones.is_empty() {
        zone_state.select(Some(app.selected_zone));
    }

    let zones_list = List::new(zone_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(zones_title)
                .border_style(match app.focus {
                    Focus::Zones => Style::default().fg(Color::Cyan),
                    _ => Style::default(),
                }),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol("→ ");

    frame.render_stateful_widget(zones_list, vertical[0], &mut zone_state);

    let paged = app.paged_records();
    let start_index = app.record_page * app.page_size();
    let rows = paged.iter().enumerate().map(|(i, record)| {
        let global_index = start_index + i;
        let mut row = Row::new(vec![
            record.record_type.clone(),
            record.name.clone(),
            record.content.clone(),
            record.ttl.to_string(),
            if record.proxied {
                "Proxied"
            } else {
                "DNS only"
            }
            .to_string(),
        ]);

        if app.focus == Focus::Records && global_index == app.selected_record {
            row = row.style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        }
        row
    });

    let border_style = if app.focus == Focus::Records {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Percentage(25),
            Constraint::Percentage(40),
            Constraint::Length(6),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["Type", "Name", "Content", "TTL", "Mode"]).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title("DNS Records"),
    )
    .column_spacing(1);

    frame.render_widget(table, vertical[1]);
}

fn draw_status<B: DnsBackend>(frame: &mut Frame<'_>, area: ratatui::prelude::Rect, app: &App<B>) {
    let (line1, line2) = app.status_message();
    let footer = Paragraph::new(vec![Line::raw(line1), Line::raw(line2)])
        .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(footer, area);
}

fn draw_account_form(frame: &mut Frame<'_>, form: &AccountForm) {
    let area = centered_rect(70, 50, frame.size());
    let mut lines = vec![
        Line::from(Span::styled(
            "Add a Cloudflare account",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("Enter to advance/submit • Tab to move • Esc to cancel"),
        Line::from(""),
        form_line("Account Name", &form.name, form.field_index == 0, true),
        form_line("API Token", &form.api_token, form.field_index == 1, true),
        form_line(
            "Email (optional)",
            &form.email,
            form.field_index == 2,
            false,
        ),
        form_line(
            "Account ID (optional, needed for scoped tokens)",
            &form.account_id,
            form.field_index == 3,
            false,
        ),
        Line::from(""),
    ];

    if !form.is_ready() {
        lines.push(Line::from("Name and API token are required."));
    } else {
        lines.push(Line::from("Press Enter on the last field to save."));
    }

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Onboarding: Add Account"),
    );

    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn draw_record_form(frame: &mut Frame<'_>, form: &RecordForm) {
    let area = centered_rect(70, 60, frame.size());
    let labels = ["Name", "Type", "Content", "TTL", "Proxied"];
    let values = [
        form.draft.name.clone(),
        form.draft.record_type.clone(),
        form.draft.content.clone(),
        form.draft.ttl.clone(),
        if form.draft.proxied {
            "true".to_string()
        } else {
            "false".to_string()
        },
    ];

    let mut lines = vec![
        Line::from(Span::styled(
            if form.is_edit {
                "Edit DNS record"
            } else {
                "Create DNS record"
            },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("Enter to advance/submit • Tab to move • Space toggles proxied • Esc to cancel"),
        Line::from(""),
    ];

    for (idx, label) in labels.iter().enumerate() {
        let active = idx == form.field_index;
        let mut display = values[idx].clone();
        if display.is_empty() {
            display = "<required>".to_string();
        }
        let style = if active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label}: "), style),
            Span::styled(display, style),
        ]));
    }

    let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(
        if form.is_edit {
            "Edit record"
        } else {
            "Create record"
        },
    ));

    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn draw_confirm_delete(frame: &mut Frame<'_>, confirm: &ConfirmDelete) {
    let area = centered_rect(60, 30, frame.size());
    let lines = vec![
        Line::from(Span::styled(
            "Confirm delete",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Delete record {}?", confirm.record_name)),
        Line::from("Enter to confirm • Esc to cancel"),
    ];
    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Delete record"),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn draw_search_overlay(frame: &mut Frame<'_>, text: &str) {
    let area = centered_rect(60, 20, frame.size());
    let lines = vec![
        Line::from("Filter records (name/content/type)"),
        Line::from(vec![
            Span::styled("/ ", Style::default().fg(Color::Yellow)),
            Span::raw(text),
        ]),
        Line::from("Enter to apply • Esc to cancel"),
    ];
    let paragraph =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Search"));
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn form_line(label: &str, value: &str, active: bool, required: bool) -> Line<'static> {
    let mut display = if value.is_empty() {
        if required {
            "<required>".to_string()
        } else {
            "<optional>".to_string()
        }
    } else {
        value.to_string()
    };

    if !required && value.is_empty() {
        display.push_str(" (press Enter to skip)");
    }

    let style = if active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    Line::from(vec![
        Span::styled(format!("{label}: "), style),
        Span::styled(display, style),
    ])
}

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    r: ratatui::prelude::Rect,
) -> ratatui::prelude::Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ]
            .as_ref(),
        )
        .split(r);

    let vertical = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            [
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ]
            .as_ref(),
        )
        .split(popup_layout[1]);

    vertical[1]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Account {
    name: String,
    api_token: String,
    email: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    auth_mode: AuthMode,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AuthMode {
    #[default]
    Token,
    GlobalKey,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Zone {
    id: String,
    name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DnsRecord {
    id: String,
    name: String,
    record_type: String,
    content: String,
    ttl: u32,
    proxied: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Config {
    accounts: Vec<Account>,
}

impl Config {
    fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let contents = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err.into()),
        };

        Ok(serde_json::from_str(&contents)?)
    }

    fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, text)?;
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Focus {
    Accounts,
    Zones,
    Records,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    AddingAccount(AccountForm),
    RecordForm(RecordForm),
    ConfirmDelete(ConfirmDelete),
    Searching(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AccountForm {
    name: String,
    api_token: String,
    email: String,
    account_id: String,
    field_index: usize,
}

impl AccountForm {
    fn active_value_mut(&mut self) -> &mut String {
        match self.field_index {
            0 => &mut self.name,
            1 => &mut self.api_token,
            2 => &mut self.email,
            _ => &mut self.account_id,
        }
    }

    fn next_field(&mut self) {
        self.field_index = (self.field_index + 1).min(3);
    }

    fn previous_field(&mut self) {
        if self.field_index == 0 {
            self.field_index = 0;
        } else {
            self.field_index -= 1;
        }
    }

    fn insert_char(&mut self, c: char) {
        self.active_value_mut().push(c);
    }

    fn backspace(&mut self) {
        self.active_value_mut().pop();
    }

    fn is_ready(&self) -> bool {
        !self.name.trim().is_empty() && !self.api_token.trim().is_empty()
    }

    fn build_account(&self) -> Result<Account, &'static str> {
        if !self.is_ready() {
            return Err("Name and API token are required");
        }
        Ok(Account {
            name: self.name.trim().to_string(),
            api_token: self.api_token.trim().to_string(),
            email: if self.email.trim().is_empty() {
                None
            } else {
                Some(self.email.trim().to_string())
            },
            account_id: if self.account_id.trim().is_empty() {
                None
            } else {
                Some(self.account_id.trim().to_string())
            },
            auth_mode: AuthMode::Token,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordForm {
    draft: RecordDraft,
    field_index: usize,
    is_edit: bool,
    target_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordDraft {
    name: String,
    record_type: String,
    content: String,
    ttl: String,
    proxied: bool,
}

impl Default for RecordDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            record_type: "A".to_string(),
            content: String::new(),
            ttl: "300".to_string(),
            proxied: true,
        }
    }
}

impl RecordDraft {
    fn from_record(record: &DnsRecord) -> Self {
        Self {
            name: record.name.clone(),
            record_type: record.record_type.clone(),
            content: record.content.clone(),
            ttl: record.ttl.to_string(),
            proxied: record.proxied,
        }
    }

    fn to_record(&self, id: String) -> Result<DnsRecord> {
        let ttl: u32 = self
            .ttl
            .trim()
            .parse()
            .map_err(|_| anyhow!("TTL must be a number"))?;
        if self.name.trim().is_empty() || self.record_type.trim().is_empty() {
            return Err(anyhow!("Name and type are required"));
        }
        Ok(DnsRecord {
            id,
            name: self.name.trim().to_string(),
            record_type: self.record_type.trim().to_string(),
            content: self.content.trim().to_string(),
            ttl,
            proxied: self.proxied,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfirmDelete {
    record_id: String,
    record_name: String,
}

struct App<B: DnsBackend> {
    config_path: PathBuf,
    backend: B,
    accounts: Vec<Account>,
    selected_account: usize,
    zones: Vec<Zone>,
    selected_zone: usize,
    selected_record: usize,
    records: Vec<DnsRecord>,
    focus: Focus,
    mode: Mode,
    record_filter: String,
    record_page: usize,
    record_page_size: usize,
    last_message: String,
}

impl<B: DnsBackend> App<B> {
    fn new(config_path: impl Into<PathBuf>, accounts: Vec<Account>, backend: B) -> Result<Self> {
        let mut app = Self {
            config_path: config_path.into(),
            backend,
            accounts,
            selected_account: 0,
            zones: Vec::new(),
            selected_zone: 0,
            selected_record: 0,
            records: Vec::new(),
            focus: Focus::Accounts,
            mode: Mode::Normal,
            record_filter: String::new(),
            record_page: 0,
            record_page_size: 10,
            last_message: String::new(),
        };

        app.refresh_current()?;
        if app.accounts.is_empty() {
            app.mode = Mode::AddingAccount(AccountForm::default());
            app.last_message = "Add your first Cloudflare account (name + API token).".to_string();
        }

        Ok(app)
    }

    fn current_account(&self) -> Option<&Account> {
        self.accounts.get(self.selected_account)
    }

    fn current_zone(&self) -> Option<&Zone> {
        self.zones.get(self.selected_zone)
    }

    fn next_account(&mut self) -> Result<()> {
        if self.accounts.is_empty() {
            return Ok(());
        }
        self.selected_account = (self.selected_account + 1) % self.accounts.len();
        self.selected_zone = 0;
        self.refresh_current()
    }

    fn previous_account(&mut self) -> Result<()> {
        if self.accounts.is_empty() {
            return Ok(());
        }

        if self.selected_account == 0 {
            self.selected_account = self.accounts.len() - 1;
        } else {
            self.selected_account -= 1;
        }
        self.selected_zone = 0;
        self.refresh_current()
    }

    fn next_zone(&mut self) -> Result<()> {
        if self.zones.is_empty() {
            return Ok(());
        }
        self.selected_zone = (self.selected_zone + 1) % self.zones.len();
        self.refresh_records()
    }

    fn previous_zone(&mut self) -> Result<()> {
        if self.zones.is_empty() {
            return Ok(());
        }
        if self.selected_zone == 0 {
            self.selected_zone = self.zones.len() - 1;
        } else {
            self.selected_zone -= 1;
        }
        self.refresh_records()
    }

    fn next_record(&mut self) {
        let total = self.filtered_records().len();
        if total == 0 {
            return;
        }
        self.selected_record = (self.selected_record + 1).min(total.saturating_sub(1));
        self.ensure_record_visible(total);
    }

    fn previous_record(&mut self) {
        let total = self.filtered_records().len();
        if total == 0 {
            return;
        }
        if self.selected_record == 0 {
            self.selected_record = 0;
        } else {
            self.selected_record -= 1;
        }
        self.ensure_record_visible(total);
    }

    fn next_page(&mut self) {
        let total = self.filtered_records().len();
        let page_count = self.record_page_count(total);
        if page_count == 0 {
            return;
        }
        self.record_page = (self.record_page + 1) % page_count;
        self.selected_record = self.record_page * self.page_size();
    }

    fn previous_page(&mut self) {
        let total = self.filtered_records().len();
        let page_count = self.record_page_count(total);
        if page_count == 0 {
            return;
        }
        if self.record_page == 0 {
            self.record_page = page_count - 1;
        } else {
            self.record_page -= 1;
        }
        self.selected_record = self.record_page * self.page_size();
    }

    fn refresh_current(&mut self) -> Result<()> {
        self.refresh_zones()?;
        self.refresh_records()
    }

    fn refresh_zones(&mut self) -> Result<()> {
        if let Some(account) = self.current_account().cloned() {
            let zones = self.backend.list_zones(&account)?;
            self.zones = zones;
            if self.selected_zone >= self.zones.len() {
                self.selected_zone = 0;
            }
            self.last_message = format!("Loaded {} zone(s) for {}", self.zones.len(), account.name);
        } else {
            self.zones.clear();
            self.records.clear();
        }
        Ok(())
    }

    fn refresh_records(&mut self) -> Result<()> {
        match (
            self.current_account().cloned(),
            self.current_zone().cloned(),
        ) {
            (Some(account), Some(zone)) => {
                let records = self.backend.list_records(&account, &zone)?;
                self.records = records;
                self.last_message = format!("{} record(s) in {}", self.records.len(), zone.name);
            }
            _ => self.records.clear(),
        }
        self.selected_record = 0;
        self.record_page = 0;
        Ok(())
    }

    fn filtered_records(&self) -> Vec<&DnsRecord> {
        if self.record_filter.trim().is_empty() {
            return self.records.iter().collect();
        }
        let needle = self.record_filter.to_lowercase();
        self.records
            .iter()
            .filter(|r| {
                r.name.to_lowercase().contains(&needle)
                    || r.content.to_lowercase().contains(&needle)
                    || r.record_type.to_lowercase().contains(&needle)
            })
            .collect()
    }

    fn update_record_page_size(&mut self, area_height: u16) {
        // Table uses one row for the header and two for borders.
        let usable_rows = area_height as usize;
        let new_size = usable_rows.saturating_sub(3).max(1);
        if new_size != self.record_page_size {
            self.record_page_size = new_size;
            let total = self.filtered_records().len();
            let page_count = self.record_page_count(total);
            self.record_page = self.record_page.min(page_count.saturating_sub(1));
            self.selected_record = self.selected_record.min(total.saturating_sub(1));
            self.ensure_record_visible(total);
        }
    }

    fn page_size(&self) -> usize {
        self.record_page_size.max(1)
    }

    fn record_page_count(&self, total: usize) -> usize {
        if total == 0 {
            0
        } else {
            total.div_ceil(self.page_size())
        }
    }

    fn paged_records(&self) -> Vec<&DnsRecord> {
        let filtered = self.filtered_records();
        if filtered.is_empty() {
            return filtered;
        }
        let start = self.record_page * self.page_size();
        let end = (start + self.page_size()).min(filtered.len());
        filtered[start..end].to_vec()
    }

    fn ensure_record_visible(&mut self, total: usize) {
        if total == 0 {
            self.record_page = 0;
            self.selected_record = 0;
            return;
        }
        let page = self.selected_record / self.page_size();
        self.record_page = page.min(self.record_page_count(total).saturating_sub(1));
    }

    fn status_message(&self) -> (String, String) {
        let help = "q: quit  a: add account  r: refresh  Tab/Shift+Tab: focus  ↑/↓: move  /: search  n/e/d: new/edit/del  PgUp/PgDn: pages";
        if self.accounts.is_empty() {
            return (
                help.to_string(),
                "No accounts configured. Press 'a' to add one. Tokens are stored locally.".to_string(),
            );
        }

        let account_name = self
            .current_account()
            .map(|a| a.name.as_str())
            .unwrap_or("No account");
        let zone_name = self
            .current_zone()
            .map(|z| z.name.as_str())
            .unwrap_or("No zone");
        let account_index = if self.accounts.is_empty() {
            0
        } else {
            self.selected_account + 1
        };
        let zone_index = if self.zones.is_empty() {
            0
        } else {
            self.selected_zone + 1
        };
        let zone_total = self.zones.len().max(1);

        let filtered_count = self.filtered_records().len();
        let page_count = self.record_page_count(filtered_count).max(1);
        let filter_suffix = if self.record_filter.trim().is_empty() {
            ""
        } else {
            " filtered"
        };

        (
            help.to_string(),
            format!(
                "Account: {} ({}/{}) | Zone: {} ({}/{}) | Records: page {}/{} ({} shown{}) | {}",
                account_name,
                account_index,
                self.accounts.len(),
                zone_name,
                zone_index,
                zone_total,
                self.record_page + 1,
                page_count,
                self.paged_records().len(),
                filter_suffix,
                self.last_message
            ),
        )
    }

    fn start_add_account(&mut self) {
        self.mode = Mode::AddingAccount(AccountForm::default());
        self.last_message = "Add a Cloudflare API token for this account".to_string();
    }

    fn finish_add_account(&mut self, account: Account) -> Result<()> {
        let name = account.name.clone();
        self.accounts.push(account);
        self.selected_account = self.accounts.len().saturating_sub(1);
        self.selected_zone = 0;
        self.mode = Mode::Normal;
        self.save_accounts()?;
        self.refresh_current()?;
        self.last_message = format!("Added account {name}");
        Ok(())
    }

    fn save_accounts(&self) -> Result<()> {
        let config = Config {
            accounts: self.accounts.clone(),
        };
        config.save(&self.config_path)
    }

    fn ensure_onboarding_prompt(&mut self) {
        if self.accounts.is_empty() {
            self.start_add_account();
        }
    }

    fn start_record_form(&mut self, is_edit: bool) {
        let draft = if is_edit {
            if let Some(rec) = self.current_record() {
                RecordDraft::from_record(rec)
            } else {
                RecordDraft::default()
            }
        } else {
            RecordDraft::default()
        };
        let target_id = self.current_record().map(|r| r.id.clone());
        self.mode = Mode::RecordForm(RecordForm {
            draft,
            field_index: 0,
            is_edit,
            target_id,
        });
        self.last_message = if is_edit {
            "Editing DNS record".to_string()
        } else {
            "Create DNS record".to_string()
        };
    }

    fn ask_delete_record(&mut self) {
        if let Some(record) = self.current_record().cloned() {
            self.mode = Mode::ConfirmDelete(ConfirmDelete {
                record_id: record.id.clone(),
                record_name: record.name.clone(),
            });
            self.last_message = format!("Delete {}?", record.name);
        }
    }

    fn current_record(&self) -> Option<&DnsRecord> {
        self.filtered_records().get(self.selected_record).copied()
    }

    fn create_record(&mut self, record: DnsRecord) -> Result<()> {
        let (account, zone) = match (self.current_account(), self.current_zone()) {
            (Some(a), Some(z)) => (a.clone(), z.clone()),
            _ => return Ok(()),
        };

        let created = self
            .backend
            .create_record(&account, &zone, record.clone())?;
        self.last_message = format!("Created {}", created.name);
        self.mode = Mode::Normal;
        self.refresh_records()
    }

    fn update_record(&mut self, record: DnsRecord) -> Result<()> {
        let (account, zone) = match (self.current_account(), self.current_zone()) {
            (Some(a), Some(z)) => (a.clone(), z.clone()),
            _ => return Ok(()),
        };

        let updated = self
            .backend
            .update_record(&account, &zone, record.clone())?;
        self.last_message = format!("Updated {}", updated.name);
        self.mode = Mode::Normal;
        self.refresh_records()
    }

    fn delete_record(&mut self, record_id: String) -> Result<()> {
        let (account, zone) = match (self.current_account(), self.current_zone()) {
            (Some(a), Some(z)) => (a.clone(), z.clone()),
            _ => return Ok(()),
        };

        self.backend.delete_record(&account, &zone, &record_id)?;
        self.last_message = "Record deleted".to_string();
        self.refresh_records()
    }
}

trait DnsBackend {
    fn list_zones(&mut self, account: &Account) -> Result<Vec<Zone>>;
    fn list_records(&mut self, account: &Account, zone: &Zone) -> Result<Vec<DnsRecord>>;
    fn create_record(
        &mut self,
        account: &Account,
        zone: &Zone,
        record: DnsRecord,
    ) -> Result<DnsRecord>;
    fn update_record(
        &mut self,
        account: &Account,
        zone: &Zone,
        record: DnsRecord,
    ) -> Result<DnsRecord>;
    fn delete_record(&mut self, account: &Account, zone: &Zone, record_id: &str) -> Result<()>;
}

enum Backend {
    Cloudflare(CloudflareBackend),
    Mock(MockBackend),
}

impl DnsBackend for Backend {
    fn list_zones(&mut self, account: &Account) -> Result<Vec<Zone>> {
        match self {
            Backend::Cloudflare(client) => client.list_zones(account),
            Backend::Mock(mock) => mock.list_zones(account),
        }
    }

    fn list_records(&mut self, account: &Account, zone: &Zone) -> Result<Vec<DnsRecord>> {
        match self {
            Backend::Cloudflare(client) => client.list_records(account, zone),
            Backend::Mock(mock) => mock.list_records(account, zone),
        }
    }

    fn create_record(
        &mut self,
        account: &Account,
        zone: &Zone,
        record: DnsRecord,
    ) -> Result<DnsRecord> {
        match self {
            Backend::Cloudflare(client) => client.create_record(account, zone, record),
            Backend::Mock(mock) => mock.create_record(account, zone, record),
        }
    }

    fn update_record(
        &mut self,
        account: &Account,
        zone: &Zone,
        record: DnsRecord,
    ) -> Result<DnsRecord> {
        match self {
            Backend::Cloudflare(client) => client.update_record(account, zone, record),
            Backend::Mock(mock) => mock.update_record(account, zone, record),
        }
    }

    fn delete_record(&mut self, account: &Account, zone: &Zone, record_id: &str) -> Result<()> {
        match self {
            Backend::Cloudflare(client) => client.delete_record(account, zone, record_id),
            Backend::Mock(mock) => mock.delete_record(account, zone, record_id),
        }
    }
}

struct CloudflareBackend {
    client: Client,
    base_url: String,
}

impl CloudflareBackend {
    fn new() -> Result<Self> {
        Self::new_with_base(CF_API_BASE)
    }

    fn new_with_base(base_url: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .user_agent("nyxflare/0.1")
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.into(),
        })
    }

    fn with_auth(&self, request: RequestBuilder, account: &Account) -> RequestBuilder {
        match account.auth_mode {
            AuthMode::Token => request.bearer_auth(&account.api_token),
            AuthMode::GlobalKey => {
                if let Some(email) = &account.email {
                    request
                        .header("X-Auth-Email", email)
                        .header("X-Auth-Key", &account.api_token)
                } else {
                    request
                }
            }
        }
    }

    fn list_zones(&mut self, account: &Account) -> Result<Vec<Zone>> {
        let url = format!("{}/zones", self.base_url);
        let response = self
            .with_auth(self.client.get(url), account)
            .query(&self.zone_query(account))
            .send()
            .with_context(|| format!("Listing zones for {}", account.name))?;

        let status = response.status();
        let text = response.text().unwrap_or_default();
        let parsed: CfResponse<CfZone> = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse Cloudflare zones response: {text}"))?;

        if !status.is_success() || !parsed.success {
            return Err(anyhow!(format!(
                "Zones ({status}): {} | body: {}",
                parsed.error_message(),
                truncate_body(&text)
            )));
        }

        Ok(parsed
            .result
            .unwrap_or_default()
            .into_iter()
            .map(|z| Zone {
                id: z.id,
                name: z.name,
            })
            .collect())
    }

    fn list_records(&mut self, account: &Account, zone: &Zone) -> Result<Vec<DnsRecord>> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, zone.id);
        let response = self
            .with_auth(self.client.get(url), account)
            .query(&[("per_page", 200)])
            .send()
            .with_context(|| format!("Listing records for zone {}", zone.name))?;

        let status = response.status();
        let text = response.text().unwrap_or_default();
        let parsed: CfResponse<CfRecord> = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse Cloudflare dns_records response: {text}"))?;

        if !status.is_success() || !parsed.success {
            return Err(anyhow!(format!(
                "Records ({status}): {} | body: {}",
                parsed.error_message(),
                truncate_body(&text)
            )));
        }

        Ok(parsed
            .result
            .unwrap_or_default()
            .into_iter()
            .map(|r| DnsRecord {
                id: r.id,
                name: r.name,
                record_type: r.record_type,
                content: r.content,
                ttl: r.ttl.unwrap_or(300),
                proxied: r.proxied.unwrap_or(false),
            })
            .collect())
    }

    fn create_record(
        &mut self,
        account: &Account,
        zone: &Zone,
        record: DnsRecord,
    ) -> Result<DnsRecord> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, zone.id);
        let response = self
            .with_auth(self.client.post(url), account)
            .json(&CfRecordWrite::from_record(&record))
            .send()
            .with_context(|| format!("Creating record {}", record.name))?;

        let status = response.status();
        let text = response.text().unwrap_or_default();
        let parsed: CfItemResponse<CfRecord> = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse create record response: {text}"))?;

        if !status.is_success() || !parsed.success {
            return Err(anyhow!(format!(
                "Create ({status}): {} | body: {}",
                parsed.error_message(),
                truncate_body(&text)
            )));
        }

        let result = parsed
            .result
            .ok_or_else(|| anyhow!("Create succeeded but missing result"))?;
        Ok(result.into_dns_record())
    }

    fn update_record(
        &mut self,
        account: &Account,
        zone: &Zone,
        record: DnsRecord,
    ) -> Result<DnsRecord> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base_url, zone.id, record.id
        );
        let response = self
            .with_auth(self.client.put(url), account)
            .json(&CfRecordWrite::from_record(&record))
            .send()
            .with_context(|| format!("Updating record {}", record.name))?;

        let status = response.status();
        let text = response.text().unwrap_or_default();
        let parsed: CfItemResponse<CfRecord> = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse update record response: {text}"))?;

        if !status.is_success() || !parsed.success {
            return Err(anyhow!(format!(
                "Update ({status}): {} | body: {}",
                parsed.error_message(),
                truncate_body(&text)
            )));
        }

        let result = parsed
            .result
            .ok_or_else(|| anyhow!("Update succeeded but missing result"))?;
        Ok(result.into_dns_record())
    }

    fn delete_record(&mut self, account: &Account, zone: &Zone, record_id: &str) -> Result<()> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base_url, zone.id, record_id
        );
        let response = self
            .with_auth(self.client.delete(url), account)
            .send()
            .with_context(|| format!("Deleting record {}", record_id))?;

        let status = response.status();
        let text = response.text().unwrap_or_default();
        let parsed: CfDeleteResponse = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse delete record response: {text}"))?;

        if !status.is_success() || !parsed.success {
            return Err(anyhow!(format!(
                "Delete ({status}): {} | body: {}",
                parsed.error_message(),
                truncate_body(&text)
            )));
        }

        Ok(())
    }

    fn zone_query(&self, account: &Account) -> Vec<(&'static str, String)> {
        let mut params = vec![("per_page", "200".to_string())];
        if let Some(account_id) = &account.account_id {
            params.push(("account.id", account_id.clone()));
        }
        params
    }
}

#[derive(Deserialize)]
struct CfResponse<T> {
    success: bool,
    errors: Vec<CfError>,
    result: Option<Vec<T>>,
}

impl<T> CfResponse<T> {
    fn error_message(&self) -> String {
        self.errors
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "Unknown Cloudflare API error".to_string())
    }
}

#[derive(Deserialize)]
struct CfItemResponse<T> {
    success: bool,
    errors: Vec<CfError>,
    result: Option<T>,
}

impl<T> CfItemResponse<T> {
    fn error_message(&self) -> String {
        self.errors
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "Unknown Cloudflare API error".to_string())
    }
}

#[derive(Deserialize)]
struct CfDeleteResponse {
    success: bool,
    errors: Vec<CfError>,
}

impl CfDeleteResponse {
    fn error_message(&self) -> String {
        self.errors
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "Unknown Cloudflare API error".to_string())
    }
}

fn truncate_body(text: &str) -> String {
    const LIMIT: usize = 200;
    if text.len() > LIMIT {
        format!("{}...", &text[..LIMIT])
    } else {
        text.to_string()
    }
}

#[derive(Deserialize)]
struct CfError {
    message: String,
}

#[derive(Deserialize)]
struct CfZone {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct CfRecord {
    id: String,
    name: String,
    #[serde(rename = "type")]
    record_type: String,
    content: String,
    ttl: Option<u32>,
    proxied: Option<bool>,
}

impl CfRecord {
    fn into_dns_record(self) -> DnsRecord {
        DnsRecord {
            id: self.id,
            name: self.name,
            record_type: self.record_type,
            content: self.content,
            ttl: self.ttl.unwrap_or(300),
            proxied: self.proxied.unwrap_or(false),
        }
    }
}

#[derive(Serialize)]
struct CfRecordWrite {
    name: String,
    #[serde(rename = "type")]
    record_type: String,
    content: String,
    ttl: u32,
    proxied: bool,
}

impl CfRecordWrite {
    fn from_record(record: &DnsRecord) -> Self {
        Self {
            name: record.name.clone(),
            record_type: record.record_type.clone(),
            content: record.content.clone(),
            ttl: record.ttl,
            proxied: record.proxied,
        }
    }
}

struct MockBackend {
    records: HashMap<String, Vec<DnsRecord>>,
}

impl MockBackend {
    fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    fn ensure_zone(&mut self, zone: &Zone) {
        self.records.entry(zone.id.clone()).or_insert_with(|| {
            vec![
                DnsRecord {
                    id: format!("{}-a", zone.id),
                    name: format!("api.{}", zone.name),
                    record_type: "A".to_string(),
                    content: "203.0.113.10".to_string(),
                    ttl: 300,
                    proxied: true,
                },
                DnsRecord {
                    id: format!("{}-b", zone.id),
                    name: format!("cdn.{}", zone.name),
                    record_type: "CNAME".to_string(),
                    content: "edge.service.net".to_string(),
                    ttl: 120,
                    proxied: true,
                },
                DnsRecord {
                    id: format!("{}-c", zone.id),
                    name: format!("mail.{}", zone.name),
                    record_type: "MX".to_string(),
                    content: "mail.{zone}".replace("{zone}", &zone.name),
                    ttl: 3600,
                    proxied: false,
                },
            ]
        });
    }
}

impl DnsBackend for MockBackend {
    fn list_zones(&mut self, account: &Account) -> Result<Vec<Zone>> {
        // Generate deterministic mock zones based on account name so the UI feels connected.
        let base = account.name.replace(' ', "").to_lowercase();
        let zones = vec![
            Zone {
                id: format!("{}-01", base),
                name: format!("{}.example.com", base),
            },
            Zone {
                id: format!("{}-02", base),
                name: format!("{}.services.io", base),
            },
        ];
        Ok(zones)
    }

    fn list_records(&mut self, _account: &Account, zone: &Zone) -> Result<Vec<DnsRecord>> {
        self.ensure_zone(zone);
        Ok(self.records.get(&zone.id).cloned().unwrap_or_default())
    }

    fn create_record(
        &mut self,
        _account: &Account,
        zone: &Zone,
        record: DnsRecord,
    ) -> Result<DnsRecord> {
        self.ensure_zone(zone);
        let records = self.records.entry(zone.id.clone()).or_default();
        let mut new_record = record;
        new_record.id = format!("{}-{}", zone.id, records.len() + 1);
        records.push(new_record.clone());
        Ok(new_record)
    }

    fn update_record(
        &mut self,
        _account: &Account,
        zone: &Zone,
        record: DnsRecord,
    ) -> Result<DnsRecord> {
        self.ensure_zone(zone);
        if let Some(records) = self.records.get_mut(&zone.id)
            && let Some(existing) = records.iter_mut().find(|r| r.id == record.id)
        {
            *existing = record.clone();
        }
        Ok(record)
    }

    fn delete_record(&mut self, _account: &Account, zone: &Zone, record_id: &str) -> Result<()> {
        if let Some(records) = self.records.get_mut(&zone.id) {
            records.retain(|r| r.id != record_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("cloudflare_tui_test_{name}_{nanos}.json"))
    }

    fn test_account() -> Account {
        Account {
            name: "demo".to_string(),
            api_token: "token".to_string(),
            email: None,
            account_id: None,
            auth_mode: AuthMode::Token,
        }
    }

    fn record(id: &str, name: &str, record_type: &str, content: &str) -> DnsRecord {
        DnsRecord {
            id: id.to_string(),
            name: name.to_string(),
            record_type: record_type.to_string(),
            content: content.to_string(),
            ttl: 300,
            proxied: false,
        }
    }

    fn app_with_records(test_name: &str, records: Vec<DnsRecord>) -> App<MockBackend> {
        let mut backend = MockBackend {
            records: HashMap::new(),
        };
        backend
            .records
            .insert("demo-01".to_string(), records.clone());

        let mut app = App::new(temp_config_path(test_name), vec![test_account()], backend).unwrap();
        app.records = records;
        app.record_page = 0;
        app.selected_record = 0;
        app
    }

    #[test]
    fn add_account_form_allows_typing_command_keys() {
        let mut app = App::new(temp_config_path("add_form"), vec![], MockBackend::new()).unwrap();
        let quit = handle_add_account_key(KeyCode::Char('q'), &mut app).unwrap();

        assert!(!quit, "q should not quit while typing");
        if let Mode::AddingAccount(form) = app.mode {
            assert_eq!(form.name, "q");
        } else {
            panic!("app left add account mode");
        }
    }

    #[test]
    fn normal_mode_can_start_add_account_when_accounts_exist() {
        let mut app = app_with_records("add_account", vec![]);
        let quit = handle_normal_key(KeyCode::Char('a'), &mut app).unwrap();

        assert!(!quit, "a should not quit the app");
        assert!(
            matches!(app.mode, Mode::AddingAccount(_)),
            "app should enter add-account mode"
        );
    }

    #[test]
    fn record_form_allows_typing_command_keys() {
        let mut app = app_with_records("record_form", vec![]);
        app.start_record_form(false);

        let quit = handle_record_form_key(KeyCode::Char('q'), &mut app).unwrap();
        assert!(!quit, "q should not quit while editing a record");

        if let Mode::RecordForm(form) = app.mode {
            assert_eq!(form.draft.name, "q");
        } else {
            panic!("app left record form mode");
        }
    }

    #[test]
    fn search_overlay_allows_typing_command_keys() {
        let mut app = app_with_records("search_overlay", vec![]);
        app.mode = Mode::Searching(String::new());

        let quit = handle_search_key(KeyCode::Char('q'), &mut app).unwrap();
        assert!(!quit, "q should not quit while searching");

        if let Mode::Searching(text) = app.mode {
            assert_eq!(text, "q");
        } else {
            panic!("app left search mode");
        }
    }

    #[test]
    fn filtered_records_matches_across_fields() {
        let records = vec![
            record("1", "api.demo.example.com", "A", "203.0.113.1"),
            record("2", "cdn.demo.example.com", "CNAME", "api.demo.example.com"),
            record("3", "_acme.demo.example.com", "TXT", "challenge-token"),
        ];
        let mut app = app_with_records("filter", records);

        app.record_filter = "acme".to_string();
        let filtered = app.filtered_records();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].record_type, "TXT");

        app.record_filter = "api.demo".to_string();
        let filtered = app.filtered_records();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|r| r.record_type == "A"));
        assert!(filtered.iter().any(|r| r.record_type == "CNAME"));
    }

    #[test]
    fn paged_records_respects_page_and_size() {
        let records = (1..=5)
            .map(|i| record(&i.to_string(), &format!("r{i}.demo"), "A", "127.0.0.1"))
            .collect();
        let mut app = app_with_records("paging", records);

        app.update_record_page_size(5); // yields a page size of 2

        app.record_page = 0;
        let first_page = app.paged_records();
        assert_eq!(first_page.len(), 2);
        assert_eq!(first_page[0].name, "r1.demo");

        app.record_page = 1;
        let second_page = app.paged_records();
        assert_eq!(second_page.len(), 2);
        assert_eq!(second_page[0].name, "r3.demo");

        app.record_page = 2;
        let final_page = app.paged_records();
        assert_eq!(final_page.len(), 1);
        assert_eq!(final_page[0].name, "r5.demo");
    }

    #[test]
    fn ensure_record_visible_moves_page_to_selection() {
        let records = (1..=5)
            .map(|i| record(&i.to_string(), &format!("rec-{i}.demo"), "A", "127.0.0.1"))
            .collect();
        let mut app = app_with_records("visible", records);
        app.update_record_page_size(6); // page size 3

        app.record_page = 0;
        app.selected_record = 4;
        let total = app.filtered_records().len();
        app.ensure_record_visible(total);

        assert_eq!(app.record_page, 1);
        assert_eq!(app.selected_record, 4);
    }

    #[test]
    fn update_record_page_size_clamps_page_and_selection() {
        let records = (1..=5)
            .map(|i| record(&i.to_string(), &format!("rec-{i}.demo"), "A", "127.0.0.1"))
            .collect();
        let mut app = app_with_records("clamp", records);

        app.record_page_size = 1;
        app.record_page = 4;
        app.selected_record = 4;

        app.update_record_page_size(12); // page size grows to 9, shrinking page count to 1

        assert_eq!(app.page_size(), 9);
        assert_eq!(app.record_page, 0);
        assert_eq!(app.selected_record, 4);
    }

    #[test]
    fn status_message_reports_filter_and_paging() {
        let records = (1..=3)
            .map(|i| record(&i.to_string(), &format!("item-{i}.demo"), "A", "127.0.0.1"))
            .collect();
        let mut app = app_with_records("status", records);

        app.update_record_page_size(5); // page size 2
        app.record_filter = "item".to_string();
        let (_, details) = app.status_message();

        assert!(
            details.contains("Records: page 1/2"),
            "status did not include page count: {}",
            details
        );
        assert!(details.contains("filtered"), "status did not mark filter");
    }

    fn cf_account() -> Account {
        Account {
            name: "cf".to_string(),
            api_token: "cf-token".to_string(),
            email: Some("user@example.com".to_string()),
            account_id: Some("acc-1".to_string()),
            auth_mode: AuthMode::Token,
        }
    }

    fn cf_zone() -> Zone {
        Zone {
            id: "zone-1".to_string(),
            name: "example.com".to_string(),
        }
    }

    #[test]
    fn cloudflare_list_zones_parses_success() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/zones")
            .match_header("authorization", "Bearer cf-token")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("per_page".into(), "200".into()),
                mockito::Matcher::UrlEncoded("account.id".into(), "acc-1".into()),
            ]))
            .with_status(200)
            .with_body(
                json!({
                    "success": true,
                    "errors": [],
                    "result": [
                        {"id": "zone-1", "name": "example.com"},
                        {"id": "zone-2", "name": "demo.net"}
                    ]
                })
                .to_string(),
            )
            .create();

        let mut backend = CloudflareBackend::new_with_base(server.url()).unwrap();
        let zones = backend.list_zones(&cf_account()).unwrap();
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].name, "example.com");
    }

    #[test]
    fn cloudflare_list_records_parses_success() {
        let mut server = mockito::Server::new();
        let zone = cf_zone();
        let path = format!("/zones/{}/dns_records", zone.id);
        let _m = server
            .mock("GET", path.as_str())
            .match_header("authorization", "Bearer cf-token")
            .match_query(mockito::Matcher::UrlEncoded("per_page".into(), "200".into()))
            .with_status(200)
            .with_body(
                json!({
                    "success": true,
                    "errors": [],
                    "result": [
                        {"id": "rec-1", "name": "api.example.com", "type": "A", "content": "1.1.1.1", "ttl": 120, "proxied": true},
                        {"id": "rec-2", "name": "edge.example.com", "type": "CNAME", "content": "api.example.com", "ttl": 300, "proxied": false}
                    ]
                })
                .to_string(),
            )
            .create();

        let mut backend = CloudflareBackend::new_with_base(server.url()).unwrap();
        let records = backend.list_records(&cf_account(), &zone).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, "A");
        assert!(records[0].proxied);
        assert_eq!(records[1].record_type, "CNAME");
    }

    #[test]
    fn cloudflare_create_update_delete_roundtrip() {
        let mut server = mockito::Server::new();
        let zone = cf_zone();

        let create_path = format!("/zones/{}/dns_records", zone.id);
        let _create = server
            .mock("POST", create_path.as_str())
            .match_header("authorization", "Bearer cf-token")
            .match_body(mockito::Matcher::PartialJson(json!({
                "name": "new.example.com",
                "type": "A",
                "content": "2.2.2.2",
                "ttl": 300,
                "proxied": false
            })))
            .with_status(200)
            .with_body(
                json!({
                    "success": true,
                    "errors": [],
                    "result": {
                        "id": "rec-new",
                        "name": "new.example.com",
                        "type": "A",
                        "content": "2.2.2.2",
                        "ttl": 300,
                        "proxied": false
                    }
                })
                .to_string(),
            )
            .create();

        let update_path = format!("/zones/{}/dns_records/rec-new", zone.id);
        let _update = server
            .mock("PUT", update_path.as_str())
            .match_body(mockito::Matcher::PartialJson(json!({"content": "3.3.3.3"})))
            .with_status(200)
            .with_body(
                json!({
                    "success": true,
                    "errors": [],
                    "result": {
                        "id": "rec-new",
                        "name": "new.example.com",
                        "type": "A",
                        "content": "3.3.3.3",
                        "ttl": 300,
                        "proxied": false
                    }
                })
                .to_string(),
            )
            .create();

        let delete_path = format!("/zones/{}/dns_records/rec-new", zone.id);
        let _delete = server
            .mock("DELETE", delete_path.as_str())
            .with_status(200)
            .with_body(json!({"success": true, "errors": []}).to_string())
            .create();

        let mut backend = CloudflareBackend::new_with_base(server.url()).unwrap();
        let mut created = backend
            .create_record(
                &cf_account(),
                &zone,
                record("rec-new", "new.example.com", "A", "2.2.2.2"),
            )
            .unwrap();
        assert_eq!(created.id, "rec-new");

        created.content = "3.3.3.3".to_string();
        let updated = backend
            .update_record(&cf_account(), &zone, created)
            .unwrap();
        assert_eq!(updated.content, "3.3.3.3");

        backend
            .delete_record(&cf_account(), &zone, "rec-new")
            .unwrap();
    }

    #[test]
    fn cloudflare_errors_propagate_context() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/zones")
            .match_query(mockito::Matcher::Any)
            .with_status(500)
            .with_body(
                json!({
                    "success": false,
                    "errors": [{"message": "boom"}],
                    "result": null
                })
                .to_string(),
            )
            .create();

        let mut backend = CloudflareBackend::new_with_base(server.url()).unwrap();
        let err = backend.list_zones(&cf_account()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("boom") || msg.contains("Zones"),
            "error lacked context: {msg}"
        );
    }
}
