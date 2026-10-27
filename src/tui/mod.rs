use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::Config;
use crate::diff::excerpt;
use crate::engine;
use crate::engine::DocumentKind;
use crate::matching::Progress;
use crate::report;
use crate::result::{CompareOutput, DiffOp, DiffSpan, Record, Status};
use crate::revision::{self, RevisionBlock, RevisionKind, TableSummary};

mod entry;
mod picker;
mod xlsx;

enum WorkerMsg {
    Progress(Progress),
    Done(Box<CompareOutput>),
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
struct DetailPanelSpec {
    pane: DetailPane,
    title_color: Color,
    active: bool,
}

/// 详情两栏的换行行数缓存。换行计算会遍历整段文本，不能在每帧重复做。
#[derive(Debug, Clone, Copy)]
struct DetailMetrics {
    record_id: usize,
    expanded: bool,
    old_width: u16,
    new_width: u16,
    height: u16,
    old_max: u16,
    new_max: u16,
}

/// 修订视图整篇换行行数的缓存，只在终端尺寸变化时重算。
#[derive(Debug, Clone, Copy)]
struct RevisionMetrics {
    width: u16,
    height: u16,
    max: u16,
}

enum State {
    Running {
        stage: String,
        step: Option<(String, usize, usize)>,
    },
    Ready(Box<CompareOutput>),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Modified,
    Suspected,
    Added,
    Removed,
    Moved,
}

impl Filter {
    const ORDER: [Filter; 6] = [
        Filter::All,
        Filter::Modified,
        Filter::Suspected,
        Filter::Added,
        Filter::Removed,
        Filter::Moved,
    ];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "全部",
            Filter::Modified => "修改",
            Filter::Suspected => "疑似",
            Filter::Added => "新增",
            Filter::Removed => "删除",
            Filter::Moved => "移动",
        }
    }

    fn count(self, records: &[Record]) -> usize {
        records
            .iter()
            .filter(|record| self.matches(record.status))
            .count()
    }

    fn matches(self, status: Status) -> bool {
        match self {
            Filter::All => true,
            Filter::Modified => status == Status::Modified,
            Filter::Suspected => status == Status::SuspectedModified,
            Filter::Added => status == Status::Added,
            Filter::Removed => status == Status::Removed,
            Filter::Moved => status == Status::Moved,
        }
    }

    fn cycle(self, delta: isize) -> Self {
        let index = Self::ORDER
            .iter()
            .position(|item| *item == self)
            .unwrap_or(0) as isize;
        let next = (index + delta).rem_euclid(Self::ORDER.len() as isize) as usize;
        Self::ORDER[next]
    }

    fn from_number(number: char) -> Option<Self> {
        match number {
            '1' => Some(Filter::All),
            '2' => Some(Filter::Modified),
            '3' => Some(Filter::Suspected),
            '4' => Some(Filter::Added),
            '5' => Some(Filter::Removed),
            '6' => Some(Filter::Moved),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Records,
    Revision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Records,
    Detail(DetailPane),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailPane {
    Old,
    New,
}

impl DetailPane {
    fn label(self) -> &'static str {
        match self {
            DetailPane::Old => "旧文",
            DetailPane::New => "新文",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportKind {
    Html,
    Markdown,
}

impl ExportKind {
    fn label(self) -> &'static str {
        match self {
            ExportKind::Html => "HTML",
            ExportKind::Markdown => "Markdown",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            ExportKind::Html => "html",
            ExportKind::Markdown => "md",
        }
    }
}

enum ExportDialog {
    Choice {
        selected: usize,
    },
    Path {
        kind: ExportKind,
        value: String,
        error: Option<String>,
    },
}

struct StatusMessage {
    text: String,
    error: bool,
}

struct App {
    old: PathBuf,
    new: PathBuf,
    state: State,
    view: ViewMode,
    filter: Filter,
    selected: usize,
    table_state: TableState,
    focus: Focus,
    expanded: bool,
    old_scroll: u16,
    new_scroll: u16,
    old_max_scroll: u16,
    new_max_scroll: u16,
    dialog: Option<ExportDialog>,
    help: bool,
    status: Option<StatusMessage>,
    quit: bool,
    fatal: bool,
    tick: usize,
    started: Instant,
    context_chars: usize,
    /// 记录列表预览在比较完成时预计算，避免每帧复制所有记录的完整原文。
    previews: Vec<String>,
    detail_metrics: Option<DetailMetrics>,
    revision: Vec<RevisionBlock>,
    revision_scroll: u16,
    revision_max_scroll: u16,
    revision_page: u16,
    revision_metrics: Option<RevisionMetrics>,
}

impl App {
    fn new(old: PathBuf, new: PathBuf, context_chars: usize) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            old,
            new,
            state: State::Running {
                stage: "准备比较".to_string(),
                step: None,
            },
            view: ViewMode::Records,
            filter: Filter::All,
            selected: 0,
            table_state,
            focus: Focus::Records,
            expanded: false,
            old_scroll: 0,
            new_scroll: 0,
            old_max_scroll: u16::MAX,
            new_max_scroll: u16::MAX,
            dialog: None,
            help: false,
            status: None,
            quit: false,
            fatal: false,
            tick: 0,
            started: Instant::now(),
            context_chars,
            previews: Vec::new(),
            detail_metrics: None,
            revision: Vec::new(),
            revision_scroll: 0,
            revision_max_scroll: 0,
            revision_page: 1,
            revision_metrics: None,
        }
    }

    fn apply(&mut self, message: WorkerMsg) {
        match message {
            WorkerMsg::Progress(Progress::Stage(stage)) => {
                if matches!(self.state, State::Running { .. }) {
                    self.state = State::Running { stage, step: None };
                }
            }
            WorkerMsg::Progress(Progress::Step { label, done, total }) => {
                if let State::Running { stage, .. } = &self.state {
                    let stage = stage.clone();
                    self.state = State::Running {
                        stage,
                        step: Some((label, done, total)),
                    };
                }
            }
            WorkerMsg::Done(output) => {
                self.previews = output
                    .records
                    .iter()
                    .map(|record| record_preview(record, 120))
                    .collect();
                self.revision = revision::build(&output.records);
                self.state = State::Ready(output);
                self.view = ViewMode::Records;
                self.selected = 0;
                self.table_state.select(Some(0));
                self.focus = Focus::Records;
                self.reset_detail_scroll();
                self.detail_metrics = None;
                self.revision_scroll = 0;
                self.revision_max_scroll = 0;
                self.revision_metrics = None;
                self.expanded = false;
                self.status = Some(StatusMessage {
                    text: "比较完成".to_string(),
                    error: false,
                });
            }
            WorkerMsg::Failed(error) => {
                log::error!("TUI 比较失败：{error}");
                self.state = State::Failed(error);
                self.fatal = true;
            }
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        match &self.state {
            State::Ready(output) => output
                .records
                .iter()
                .enumerate()
                .filter(|(_, record)| self.filter.matches(record.status))
                .map(|(index, _)| index)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn sync_selection(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
            self.table_state.select(None);
        } else {
            self.selected = self.selected.min(count - 1);
            self.table_state.select(Some(self.selected));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.filtered_indices().len();
        if count == 0 {
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        self.selected = next;
        self.table_state.select(Some(next));
        self.reset_detail_scroll();
    }

    fn select_first(&mut self) {
        self.selected = 0;
        self.table_state.select(Some(0));
        self.reset_detail_scroll();
    }

    fn select_last(&mut self) {
        let count = self.filtered_indices().len();
        if count > 0 {
            self.selected = count - 1;
            self.table_state.select(Some(self.selected));
            self.reset_detail_scroll();
        }
    }

    fn set_filter(&mut self, filter: Filter) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.selected = 0;
        self.table_state.select(Some(0));
        self.reset_detail_scroll();
        self.expanded = false;
    }

    fn reset_detail_scroll(&mut self) {
        self.old_scroll = 0;
        self.new_scroll = 0;
    }

    fn focus_detail(&mut self) {
        let Some(record) = self.current_record() else {
            return;
        };
        let pane = if record.old_text.is_some() {
            DetailPane::Old
        } else if record.new_text.is_some() {
            DetailPane::New
        } else {
            return;
        };
        self.focus = Focus::Detail(pane);
    }

    fn focus_records(&mut self) {
        self.focus = Focus::Records;
    }

    fn toggle_detail_pane(&mut self) {
        let Focus::Detail(pane) = self.focus else {
            return;
        };
        let Some(record) = self.current_record() else {
            return;
        };
        if record.old_text.is_some() && record.new_text.is_some() {
            self.focus = Focus::Detail(match pane {
                DetailPane::Old => DetailPane::New,
                DetailPane::New => DetailPane::Old,
            });
        }
    }

    fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
        self.reset_detail_scroll();
    }

    fn scroll_detail(&mut self, delta: i32) {
        let Focus::Detail(pane) = self.focus else {
            return;
        };
        let (scroll, max_scroll) = match pane {
            DetailPane::Old => (&mut self.old_scroll, self.old_max_scroll),
            DetailPane::New => (&mut self.new_scroll, self.new_max_scroll),
        };
        let next = (i32::from(*scroll) + delta).clamp(0, i32::from(max_scroll));
        *scroll = next as u16;
    }

    fn set_detail_scroll(&mut self, value: u16) {
        let Focus::Detail(pane) = self.focus else {
            return;
        };
        match pane {
            DetailPane::Old => self.old_scroll = value,
            DetailPane::New => self.new_scroll = value,
        }
    }

    fn current_record(&self) -> Option<&Record> {
        let indices = self.filtered_indices();
        let record_index = *indices.get(self.selected)?;
        match &self.state {
            State::Ready(output) => output.records.get(record_index),
            _ => None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.quit = true;
            return;
        }
        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
            ) {
                self.help = false;
            }
            return;
        }
        if self.dialog.is_some() {
            self.handle_dialog_key(key);
            return;
        }
        if matches!(self.state, State::Ready(_))
            && matches!(key.code, KeyCode::Char('v'))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.toggle_view();
            return;
        }
        if self.view == ViewMode::Revision {
            self.handle_revision_key(key);
            return;
        }
        match self.focus {
            Focus::Records => self.handle_records_key(key),
            Focus::Detail(_) => self.handle_detail_key(key),
        }
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            ViewMode::Records => ViewMode::Revision,
            ViewMode::Revision => ViewMode::Records,
        };
    }

    fn scroll_revision(&mut self, delta: isize) {
        let current = self.revision_scroll as isize;
        let max = self.revision_max_scroll as isize;
        self.revision_scroll = current.saturating_add(delta).clamp(0, max) as u16;
    }

    fn handle_revision_key(&mut self, key: KeyEvent) {
        let page = self.revision_page.max(1) as isize;
        let half = (page / 2).max(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.view = ViewMode::Records,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('e') => self.open_export(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_revision(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_revision(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_revision(-page),
            KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char(' ') => {
                self.scroll_revision(page);
            }
            KeyCode::Char('u') => self.scroll_revision(-half),
            KeyCode::Char('d') => self.scroll_revision(half),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_revision(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.scroll_revision(isize::MAX / 2),
            _ => {}
        }
    }

    fn handle_records_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                if self.expanded {
                    self.expanded = false;
                    self.reset_detail_scroll();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home => self.select_first(),
            KeyCode::End => self.select_last(),
            KeyCode::Tab => self.set_filter(self.filter.cycle(1)),
            KeyCode::BackTab => self.set_filter(self.filter.cycle(-1)),
            KeyCode::Char('f') => self.set_filter(self.filter.cycle(1)),
            KeyCode::Char(c @ '1'..='6') => {
                if let Some(filter) = Filter::from_number(c) {
                    self.set_filter(filter);
                }
            }
            KeyCode::Enter => self.focus_detail(),
            KeyCode::Char(' ') => self.toggle_expanded(),
            KeyCode::Char('e') => self.open_export(),
            _ => {}
        }
    }

    fn handle_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.focus_records(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab | KeyCode::BackTab => self.toggle_detail_pane(),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('[') => self.scroll_detail(-1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char(']') => self.scroll_detail(1),
            KeyCode::PageUp | KeyCode::Char('{') => self.scroll_detail(-10),
            KeyCode::PageDown | KeyCode::Char('}') => self.scroll_detail(10),
            KeyCode::Home => self.set_detail_scroll(0),
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_expanded(),
            KeyCode::Char('e') => self.open_export(),
            _ => {}
        }
    }

    fn open_export(&mut self) {
        if !matches!(self.state, State::Ready(_)) {
            self.status = Some(StatusMessage {
                text: "比较尚未完成，暂时不能导出".to_string(),
                error: true,
            });
            return;
        }
        self.dialog = Some(ExportDialog::Choice { selected: 0 });
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) {
        let dialog = self.dialog.take();
        let Some(mut dialog) = dialog else {
            return;
        };
        match &mut dialog {
            ExportDialog::Choice { selected } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    self.dialog = Some(dialog);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(2);
                    self.dialog = Some(dialog);
                }
                KeyCode::Enter => {
                    let kind = match *selected {
                        0 => Some(ExportKind::Html),
                        1 => Some(ExportKind::Markdown),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        self.dialog = Some(ExportDialog::Path {
                            kind,
                            value: report::default_export_path(kind.extension())
                                .display()
                                .to_string(),
                            error: None,
                        });
                    }
                }
                KeyCode::Char('h') => {
                    self.dialog = Some(ExportDialog::Path {
                        kind: ExportKind::Html,
                        value: report::default_export_path("html").display().to_string(),
                        error: None,
                    });
                }
                KeyCode::Char('m') => {
                    self.dialog = Some(ExportDialog::Path {
                        kind: ExportKind::Markdown,
                        value: report::default_export_path("md").display().to_string(),
                        error: None,
                    });
                }
                _ => {
                    self.dialog = Some(dialog);
                }
            },
            ExportDialog::Path { kind, value, error } => match key.code {
                KeyCode::Esc => {
                    self.dialog = Some(ExportDialog::Choice { selected: 0 });
                }
                KeyCode::Enter => {
                    let path = PathBuf::from(value.as_str());
                    let result = match &self.state {
                        State::Ready(output) => match kind {
                            ExportKind::Html => report::export_html(output, &path),
                            ExportKind::Markdown => report::export_markdown(output, &path),
                        },
                        _ => Ok(()),
                    };
                    match result {
                        Ok(()) => {
                            self.status = Some(StatusMessage {
                                text: format!("已导出 {}：{}", kind.label(), path.display()),
                                error: false,
                            });
                            self.dialog = None;
                        }
                        Err(err) => {
                            *error = Some(format!("{err:#}"));
                            self.dialog = Some(dialog);
                        }
                    }
                }
                KeyCode::Backspace => {
                    value.pop();
                    self.dialog = Some(dialog);
                }
                KeyCode::Delete => {
                    value.clear();
                    self.dialog = Some(dialog);
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    value.push(c);
                    self.dialog = Some(dialog);
                }
                _ => {
                    self.dialog = Some(dialog);
                }
            },
        }
    }
}

pub fn run(old: PathBuf, new: PathBuf, config: Config) -> Result<ExitCode> {
    let kind = DocumentKind::detect(&old, &new)?;
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    run_results(&mut terminal, kind, old, new, config)
}

pub fn run_with_picker(
    old: Option<PathBuf>,
    new: Option<PathBuf>,
    config: Config,
) -> Result<ExitCode> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let kind = match (old.as_deref(), new.as_deref()) {
        (Some(old), Some(new)) => DocumentKind::detect(old, new)?,
        (Some(path), None) | (None, Some(path)) => DocumentKind::from_path(path)?,
        (None, None) => match entry::choose_kind(&mut terminal)? {
            Some(kind) => kind,
            None => return Ok(ExitCode::SUCCESS),
        },
    };
    let Some((old, new)) = picker::pick(&mut terminal, kind, old, new)? else {
        return Ok(ExitCode::SUCCESS);
    };
    let kind = DocumentKind::detect(&old, &new)?;
    run_results(&mut terminal, kind, old, new, config)
}

fn run_results(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    kind: DocumentKind,
    old: PathBuf,
    new: PathBuf,
    config: Config,
) -> Result<ExitCode> {
    match kind {
        DocumentKind::Docx => run_docx_results(terminal, old, new, config),
        DocumentKind::Xlsx => xlsx::run(terminal, old, new, config),
    }
}

fn run_docx_results(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    old: PathBuf,
    new: PathBuf,
    config: Config,
) -> Result<ExitCode> {
    let ui_old = old.clone();
    let ui_new = new.clone();
    let context_chars = config.diff.context_chars;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut progress = |update| {
            let _ = sender.send(WorkerMsg::Progress(update));
        };
        match engine::docx::compare(&old, &new, &config, &mut progress) {
            Ok(output) => {
                let _ = sender.send(WorkerMsg::Done(Box::new(output)));
            }
            Err(err) => {
                let _ = sender.send(WorkerMsg::Failed(format!("{err:#}")));
            }
        }
    });

    let mut app = App::new(ui_old, ui_new, context_chars);
    event_loop(terminal, &mut app, receiver)?;
    let fatal = app.fatal;
    Ok(if fatal {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    receiver: Receiver<WorkerMsg>,
) -> Result<()> {
    loop {
        while let Ok(message) = receiver.try_recv() {
            app.apply(message);
        }
        terminal.draw(|frame| draw(frame, app))?;
        if app.quit {
            break;
        }
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }
        app.tick = app.tick.wrapping_add(1);
    }
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
            previous(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);

    draw_header(frame, chunks[0], app);
    match &app.state {
        State::Running { .. } => draw_running(frame, chunks[1], app),
        State::Failed(_) => draw_failed(frame, chunks[1], app),
        State::Ready(_) => draw_ready(frame, chunks[1], app),
    }
    draw_footer(frame, chunks[2], app);

    if app.help {
        draw_help(frame, area);
    }
    if let Some(dialog) = &app.dialog {
        draw_dialog(frame, area, dialog);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let semantic = match &app.state {
        State::Ready(output) => Some(output.semantic.short_label()),
        _ => None,
    };
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "diffRact",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(file_label(&app.old), Style::default().fg(Color::White)),
        Span::raw(" ↔ "),
        Span::styled(file_label(&app.new), Style::default().fg(Color::White)),
    ]));
    if let Some(label) = semantic {
        let enabled = matches!(&app.state, State::Ready(output) if output.semantic.enabled);
        lines.push(Line::from(Span::styled(
            if enabled {
                label
            } else {
                format!("[!] {label}")
            },
            Style::default().fg(if enabled { Color::Green } else { Color::Yellow }),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("已用时 {:.1}s", app.started.elapsed().as_secs_f64()),
            Style::default().fg(Color::DarkGray),
        )));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_running(frame: &mut Frame, area: Rect, app: &App) {
    let State::Running { stage, step } = &app.state else {
        return;
    };
    let layout = Layout::vertical([
        Constraint::Percentage(38),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);
    let spinner = ["◐", "◓", "◑", "◒"][app.tick % 4];
    let title = format!("{spinner}  {stage}");
    let text = match step {
        Some((label, done, total)) if *total > 0 => {
            format!("{label}  {done}/{total}")
        }
        _ => format!("已用时 {:.1}s", app.started.elapsed().as_secs_f64()),
    };
    let ratio = step
        .as_ref()
        .and_then(|(_, done, total)| {
            (*total > 0).then(|| (*done as f64 / *total as f64).clamp(0.0, 1.0))
        })
        .unwrap_or(0.0);
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(title))
        .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
        .ratio(ratio)
        .label(text);
    frame.render_widget(gauge, layout[1]);
    frame.render_widget(
        Paragraph::new("正在后台计算，界面保持可响应。按 q 可退出。")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        layout[3],
    );
}

fn draw_failed(frame: &mut Frame, area: Rect, app: &App) {
    let State::Failed(error) = &app.state else {
        return;
    };
    let lines = vec![
        Line::from(Span::styled(
            "比较失败",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(error.clone()),
        Line::from(""),
        Line::from("按 q 或 Esc 退出。"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("错误")
        .border_style(Style::default().fg(Color::Red));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Center),
        area,
    );
}

fn draw_ready(frame: &mut Frame, area: Rect, app: &mut App) {
    let indices = app.filtered_indices();
    app.sync_selection(indices.len());

    let layout = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
    let State::Ready(output) = &app.state else {
        return;
    };
    if app.view == ViewMode::Revision {
        draw_revision_bar(frame, layout[0], output);
        draw_revision(frame, layout[1], app);
        return;
    }
    let context_chars = app.context_chars;
    let records_focused = matches!(app.focus, Focus::Records);
    let mut filter_spans = Vec::new();
    for filter in Filter::ORDER {
        let active = filter == app.filter;
        let style = if active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if records_focused {
            Style::default().fg(Color::Gray)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        filter_spans.push(Span::styled(
            format!(" {} {} ", filter.label(), filter.count(&output.records)),
            style,
        ));
    }
    let bar = Layout::horizontal([Constraint::Min(0), Constraint::Length(26)]).split(layout[0]);
    frame.render_widget(Paragraph::new(Line::from(filter_spans)), bar[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "未变 {} · 用时 {:.2}s",
                output.summary.unchanged,
                output.elapsed.as_secs_f64()
            ),
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Right),
        bar[1],
    );

    let columns = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(layout[1]);
    let records_border = Style::default().fg(if records_focused {
        Color::Cyan
    } else {
        Color::DarkGray
    });

    let rows: Vec<Row> = indices
        .iter()
        .map(|index| {
            let record = &output.records[*index];
            let preview = app
                .previews
                .get(*index)
                .cloned()
                .unwrap_or_else(|| record_preview(record, 120));
            Row::new(vec![
                Span::styled(
                    record.status.label_zh(),
                    status_style(record.status).add_modifier(Modifier::BOLD),
                ),
                Span::raw(record.position_label(true)),
                Span::raw(record.position_label(false)),
                Span::raw(preview),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Min(3),
        ],
    )
    .header(
        Row::new(vec!["状态", "旧位置", "新位置", "内容"]).style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(38, 48, 68))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▸ ")
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(records_border)
            .title("记录"),
    );
    if indices.is_empty() {
        frame.render_widget(
            Paragraph::new("没有符合条件的记录")
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("记录")
                        .border_style(records_border),
                )
                .style(Style::default().fg(Color::DarkGray)),
            columns[0],
        );
    } else {
        frame.render_stateful_widget(table, columns[0], &mut app.table_state);
    }

    draw_detail(frame, columns[1], app, context_chars);
}

fn draw_revision_bar(frame: &mut Frame, area: Rect, output: &CompareOutput) {
    let summary = &output.summary;
    let separator = || Span::styled(" · ", Style::default().fg(Color::DarkGray));
    let spans = vec![
        Span::styled(
            "修订视图",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  以新文段落为骨架  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("未变 {}", summary.unchanged),
            Style::default().fg(Color::DarkGray),
        ),
        separator(),
        Span::styled(
            format!("修改 {}", summary.modified),
            Style::default().fg(Color::Yellow),
        ),
        separator(),
        Span::styled(
            format!("疑似 {}", summary.suspected_modified),
            Style::default().fg(Color::Magenta),
        ),
        separator(),
        Span::styled(
            format!("新增 {}", summary.added),
            Style::default().fg(Color::Green),
        ),
        separator(),
        Span::styled(
            format!("删除 {}", summary.removed),
            Style::default().fg(Color::Red),
        ),
        separator(),
        Span::styled(
            format!("移动 {}", summary.moved),
            Style::default().fg(Color::Cyan),
        ),
    ];
    let bar = Layout::horizontal([Constraint::Min(0), Constraint::Length(16)]).split(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), bar[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "v/Esc 返回记录",
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Right),
        bar[1],
    );
}

fn draw_revision(frame: &mut Frame, area: Rect, app: &mut App) {
    let lines = revision_lines(&app.revision);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(vec![
            Span::styled(
                "修订",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " · 新文骨架（删除线=旧文，下划线=新文）",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let cached = app
        .revision_metrics
        .filter(|metrics| metrics.width == inner.width && metrics.height == inner.height);
    let max = cached.map(|metrics| metrics.max).unwrap_or_else(|| {
        let total = paragraph.line_count(inner.width);
        total
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16
    });
    let scroll = app.revision_scroll.min(max);
    frame.render_widget(paragraph.scroll((scroll, 0)), inner);
    app.revision_scroll = scroll;
    app.revision_max_scroll = max;
    app.revision_page = inner.height;
    if cached.is_none() {
        app.revision_metrics = Some(RevisionMetrics {
            width: inner.width,
            height: inner.height,
            max,
        });
    }
}

fn revision_lines(blocks: &[RevisionBlock]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        append_revision_block(&mut lines, block);
    }
    lines
}

fn append_revision_block(lines: &mut Vec<Line<'static>>, block: &RevisionBlock) {
    if block.kind == RevisionKind::Table {
        if let Some(table) = block.table.as_ref() {
            lines.push(table_summary_line(table));
        }
        return;
    }

    let (tag, style) = revision_tag(block);
    let mut current = Line::from(vec![Span::styled(tag, style)]);
    if let Some(note) = block.note.as_ref() {
        current.spans.push(Span::styled(
            format!("（{note}）"),
            Style::default().fg(Color::Cyan),
        ));
    }
    current.spans.push(Span::raw(" "));
    for span in &block.spans {
        let span_style = revision_span_style(span.op);
        for (index, part) in span.text.split('\n').enumerate() {
            if index > 0 {
                lines.push(current);
                current = Line::from("");
            }
            if !part.is_empty() {
                current
                    .spans
                    .push(Span::styled(part.to_string(), span_style));
            }
        }
    }
    lines.push(current);
}

fn revision_tag(block: &RevisionBlock) -> (String, Style) {
    let old = block
        .old_index
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".to_string());
    let new = block
        .new_index
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".to_string());
    match block.kind {
        RevisionKind::Unchanged => (format!("  新{new} "), Style::default().fg(Color::DarkGray)),
        RevisionKind::Added => (
            format!("+ 新增 新{new} "),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        RevisionKind::Removed => (
            format!("- 删除 旧{old} "),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        RevisionKind::Modified => (
            format!("~ 修改 旧{old}→新{new} "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        RevisionKind::SuspectedModified => (
            format!("~ 疑似修改 旧{old}→新{new} "),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        RevisionKind::MovedFrom => (
            format!("- 移动 旧{old} "),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        RevisionKind::MovedTo => (
            format!("+ 移动 新{new} "),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        RevisionKind::Table => ("  ".to_string(), Style::default()),
    }
}

fn revision_span_style(op: DiffOp) -> Style {
    match op {
        DiffOp::Equal => Style::default(),
        DiffOp::Delete => Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::CROSSED_OUT),
        DiffOp::Insert => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::UNDERLINED),
    }
}

fn table_summary_line(table: &TableSummary) -> Line<'static> {
    let label = match table.caption.as_ref() {
        Some(caption) => {
            let caption = caption.replace(['\n', '\t'], " ");
            let caption = caption.trim();
            if caption.is_empty() {
                format!("表{}", table.table)
            } else if caption.starts_with('表') {
                caption.to_string()
            } else {
                format!("表{}（{caption}）", table.table)
            }
        }
        None => format!("表{}", table.table),
    };
    let mut spans = vec![
        Span::styled(
            "▦ ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            label,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let mut counts = Vec::new();
    if table.whole.is_some() {
        counts.push(
            match table.whole {
                Some(Status::Added) => "整表新增",
                Some(Status::Removed) => "整表删除",
                _ => "整表变化",
            }
            .to_string(),
        );
    } else {
        if table.added > 0 {
            counts.push(format!("新增 {} 行", table.added));
        }
        if table.removed > 0 {
            counts.push(format!("删除 {} 行", table.removed));
        }
        if table.modified > 0 {
            counts.push(format!("修改 {} 行", table.modified));
        }
        if table.moved > 0 {
            counts.push(format!("移动 {} 行", table.moved));
        }
        if counts.is_empty() {
            counts.push("无变化行".to_string());
        }
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        counts.join(" · "),
        Style::default().fg(Color::Yellow),
    ));
    spans.push(Span::styled(
        "（表格内容差异暂未标记）",
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &mut App, context_chars: usize) {
    let active = match app.focus {
        Focus::Detail(pane) => Some(pane),
        Focus::Records => None,
    };
    let title = match active {
        Some(pane) => format!("详情 · {}", pane.label()),
        None => "详情".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if active.is_some() {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(record) = app.current_record() else {
        frame.render_widget(
            Paragraph::new("未选择记录").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let record_id = record.id;

    let meta_height = if record.note.is_some() { 4 } else { 3 };
    let layout =
        Layout::vertical([Constraint::Length(meta_height), Constraint::Min(4)]).split(inner);
    frame.render_widget(
        Paragraph::new(detail_meta_lines(record)).wrap(Wrap { trim: false }),
        layout[0],
    );

    let panels = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(layout[1]);
    let cached = app.detail_metrics.filter(|metrics| {
        metrics.record_id == record.id
            && metrics.expanded == app.expanded
            && metrics.old_width == panels[0].width
            && metrics.new_width == panels[1].width
            && metrics.height == panels[0].height
    });
    // 只在详情获得焦点时计算完整换行行数；切换记录时不必为不可见滚动条付这笔开销。
    let compute_metrics = active.is_some() && cached.is_none();
    let (old_lines, new_lines) = comparison_lines(record, app.expanded, context_chars);
    let (old_scroll, old_max) = draw_detail_panel(
        frame,
        panels[0],
        DetailPanelSpec {
            pane: DetailPane::Old,
            title_color: old_title_color(record.status),
            active: active == Some(DetailPane::Old),
        },
        old_lines,
        app.old_scroll,
        cached.map(|metrics| metrics.old_max),
        compute_metrics,
    );
    let (new_scroll, new_max) = draw_detail_panel(
        frame,
        panels[1],
        DetailPanelSpec {
            pane: DetailPane::New,
            title_color: new_title_color(record.status),
            active: active == Some(DetailPane::New),
        },
        new_lines,
        app.new_scroll,
        cached.map(|metrics| metrics.new_max),
        compute_metrics,
    );
    app.old_scroll = old_scroll;
    app.old_max_scroll = old_max;
    app.new_scroll = new_scroll;
    app.new_max_scroll = new_max;
    if compute_metrics {
        app.detail_metrics = Some(DetailMetrics {
            record_id,
            expanded: app.expanded,
            old_width: panels[0].width,
            new_width: panels[1].width,
            height: panels[0].height,
            old_max,
            new_max,
        });
    }
}

fn detail_meta_lines(record: &Record) -> Vec<Line<'static>> {
    let semantic = record
        .scores
        .semantic
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "—".to_string());
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("#{} ", record.id),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                record.status.label_zh().to_string(),
                status_style(record.status).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "   旧 {} → 新 {}",
                record.position_label(true),
                record.position_label(false)
            )),
        ]),
        Line::from(Span::styled(
            format!(
                "文本 {:.4}   语义 {}   综合 {:.4}   matched_to {}",
                record.scores.text,
                semantic,
                record.scores.total,
                record
                    .matched_to
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "—".to_string())
            ),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if let Some(note) = &record.note {
        lines.push(Line::from(Span::styled(
            format!("备注：{note}"),
            Style::default().fg(Color::Magenta),
        )));
    }
    lines
}

fn comparison_lines(
    record: &Record,
    expanded: bool,
    context_chars: usize,
) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    match record.status {
        Status::Modified | Status::SuspectedModified => {
            if let Some(lines) = cell_comparison_lines(record, expanded, context_chars) {
                return lines;
            }
            let Some(diff) = &record.diff else {
                return (
                    plain_lines(record.old_text.as_deref(), Color::Reset),
                    plain_lines(record.new_text.as_deref(), Color::Reset),
                );
            };
            let old_spans = if expanded {
                diff.old.clone()
            } else {
                excerpt(&diff.old, context_chars)
            };
            let new_spans = if expanded {
                diff.new.clone()
            } else {
                excerpt(&diff.new, context_chars)
            };
            (spans_lines(&old_spans), spans_lines(&new_spans))
        }
        Status::Added => (
            missing_paragraph_lines("旧"),
            plain_lines(record.new_text.as_deref(), Color::Green),
        ),
        Status::Removed => (
            plain_lines(record.old_text.as_deref(), Color::Red),
            missing_paragraph_lines("新"),
        ),
        Status::Moved | Status::Unchanged => (
            plain_lines(record.old_text.as_deref(), Color::Reset),
            plain_lines(record.new_text.as_deref(), Color::Reset),
        ),
    }
}

fn cell_comparison_lines(
    record: &Record,
    expanded: bool,
    context_chars: usize,
) -> Option<(Vec<Line<'static>>, Vec<Line<'static>>)> {
    let cells = record.row_diff.as_ref()?;
    if cells.is_empty() {
        return None;
    }
    let mut old_lines = vec![Line::from(Span::styled(
        "仅显示有变化的单元格",
        Style::default().fg(Color::DarkGray),
    ))];
    let mut new_lines = old_lines.clone();
    for cell in cells {
        let old_diff = cell.diff.as_ref().map(|diff| diff.old.as_slice());
        let new_diff = cell.diff.as_ref().map(|diff| diff.new.as_slice());
        old_lines.extend(cell_side_lines(
            &cell.column,
            cell.old.as_deref(),
            old_diff,
            Color::Red,
            expanded,
            context_chars,
        ));
        new_lines.extend(cell_side_lines(
            &cell.column,
            cell.new.as_deref(),
            new_diff,
            Color::Green,
            expanded,
            context_chars,
        ));
    }
    Some((old_lines, new_lines))
}

fn cell_side_lines(
    column: &str,
    value: Option<&str>,
    spans: Option<&[DiffSpan]>,
    color: Color,
    expanded: bool,
    context_chars: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current = Line::from(vec![Span::styled(
        format!("{column}："),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )]);
    match spans {
        Some(spans) => {
            let spans = if expanded {
                spans.to_vec()
            } else {
                excerpt(spans, context_chars)
            };
            for span in spans {
                let style = match span.op {
                    DiffOp::Equal => Style::default(),
                    DiffOp::Delete => Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::CROSSED_OUT),
                    DiffOp::Insert => Style::default().fg(Color::Green),
                };
                for (index, part) in span.text.split('\n').enumerate() {
                    if index > 0 {
                        lines.push(current);
                        current = Line::from("");
                    }
                    if !part.is_empty() {
                        current.spans.push(Span::styled(part.to_string(), style));
                    }
                }
            }
        }
        None => match value {
            Some(value) => {
                for (index, part) in value.split('\n').enumerate() {
                    if index > 0 {
                        lines.push(current);
                        current = Line::from("");
                    }
                    if !part.is_empty() {
                        current
                            .spans
                            .push(Span::styled(part.to_string(), Style::default().fg(color)));
                    }
                }
            }
            None => current
                .spans
                .push(Span::styled("（无）", Style::default().fg(Color::DarkGray))),
        },
    }
    lines.push(current);
    lines
}

fn draw_detail_panel(
    frame: &mut Frame,
    area: Rect,
    spec: DetailPanelSpec,
    lines: Vec<Line<'static>>,
    scroll: u16,
    known_max: Option<u16>,
    compute_metrics: bool,
) -> (u16, u16) {
    let title = if spec.active {
        format!("▸ {}", spec.pane.label())
    } else {
        spec.pane.label().to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if spec.active {
            Color::Yellow
        } else {
            Color::DarkGray
        }))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(spec.title_color)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block);
    let max_scroll = if let Some(max) = known_max {
        max
    } else if compute_metrics {
        let total_lines = paragraph.line_count(inner.width);
        total_lines
            .saturating_sub(area.height as usize)
            .min(u16::MAX as usize) as u16
    } else {
        0
    };
    let scroll = scroll.min(max_scroll);
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
    (scroll, max_scroll)
}

fn old_title_color(status: Status) -> Color {
    match status {
        Status::Modified | Status::SuspectedModified | Status::Removed => Color::Red,
        _ => Color::DarkGray,
    }
}

fn new_title_color(status: Status) -> Color {
    match status {
        Status::Modified | Status::SuspectedModified | Status::Added => Color::Green,
        _ => Color::DarkGray,
    }
}

fn missing_paragraph_lines(side: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        format!("（无对应{side}段落）"),
        Style::default().fg(Color::DarkGray),
    ))]
}

fn plain_lines(text: Option<&str>, color: Color) -> Vec<Line<'static>> {
    let Some(text) = text else {
        return Vec::new();
    };
    text.split('\n')
        .map(|part| Line::from(Span::styled(part.to_string(), Style::default().fg(color))))
        .collect()
}

fn spans_lines(spans: &[DiffSpan]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    for span in spans {
        let style = match span.op {
            DiffOp::Equal => Style::default(),
            DiffOp::Delete => Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::CROSSED_OUT),
            DiffOp::Insert => Style::default().fg(Color::Green),
        };
        for (index, part) in span.text.split('\n').enumerate() {
            if index > 0 {
                lines.push(Line::from(""));
            }
            if !part.is_empty() {
                lines
                    .last_mut()
                    .unwrap()
                    .spans
                    .push(Span::styled(part.to_string(), style));
            }
        }
    }
    lines
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = Vec::new();
    let keys = match app.state {
        State::Ready(_) if app.view == ViewMode::Revision => {
            "焦点：修订视图   j/k 滚动   PgUp/PgDn 翻页   d/u 半页   g/G 首尾   v/Esc 返回记录   e 导出   ? 帮助   q 退出"
                .to_string()
        }
        State::Ready(_) => match app.focus {
            Focus::Records => {
                "焦点：记录   ↑↓/jk 选择   Enter 详情   v 修订视图   Tab/1-6 筛选   Space 展开   e 导出   ? 帮助   q 退出"
                    .to_string()
            }
            Focus::Detail(pane) => format!(
                "焦点：详情/{}   Tab 切栏   j/k 滚动   [ ] 细调   Enter 展开   v 修订视图   q/Esc 返回",
                pane.label()
            ),
        },
        State::Running { .. } => "计算中……   q 退出".to_string(),
        State::Failed(_) => "q / Esc 退出".to_string(),
    };
    lines.push(Line::from(Span::styled(
        keys,
        Style::default().fg(Color::DarkGray),
    )));
    if let Some(status) = &app.status {
        lines.push(Line::from(Span::styled(
            status.text.clone(),
            Style::default().fg(if status.error {
                Color::Red
            } else {
                Color::Green
            }),
        )));
    } else {
        lines.push(Line::from(""));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let rect = centered_rect(76, 86, area);
    frame.render_widget(Clear, rect);
    let lines = vec![
        Line::from(Span::styled(
            "键盘说明",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("记录列表", Style::default().fg(Color::Yellow))),
        Line::from("↑↓/jk 选择   PgUp/PgDn 翻 10 条   Home/End 首末条"),
        Line::from("Enter 进入详情   Space 展开/收起"),
        Line::from("Tab/Shift+Tab 或 f 切换筛选   1-6 直选"),
        Line::from("e 导出   ? 帮助   q 退出   Esc 收起后退出"),
        Line::from(""),
        Line::from(Span::styled("详情", Style::default().fg(Color::Yellow))),
        Line::from("Tab/Shift+Tab 切换旧文/新文（仅两侧都有内容时）"),
        Line::from("↑↓/jk 或 [ ] 滚动当前栏   { } 一次十行"),
        Line::from("Enter/Space 展开或收起   e 导出"),
        Line::from("q/Esc 返回记录列表"),
        Line::from(""),
        Line::from(Span::styled("修订视图", Style::default().fg(Color::Yellow))),
        Line::from("v 在记录/修订视图间切换   Esc 返回记录"),
        Line::from("j/k 滚动   PgUp/PgDn 或 f/b 翻页   d/u 半页   g/G 首尾"),
        Line::from("删除段与移动原位用删除线，新增与修改的新文用下划线"),
        Line::from("移动段新位置注明“移动自旧第 N 段”；表格差异暂不标记"),
        Line::from(""),
        Line::from(Span::styled(
            "按 ? 或 Esc 关闭",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("帮助")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn draw_dialog(frame: &mut Frame, area: Rect, dialog: &ExportDialog) {
    let rect = centered_rect(66, 36, area);
    frame.render_widget(Clear, rect);
    match dialog {
        ExportDialog::Choice { selected } => {
            let lines = vec![
                Line::from(Span::styled(
                    "选择导出格式",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                choice_line(0, "HTML（自包含，可直接浏览）", *selected),
                Line::from(""),
                choice_line(1, "Markdown（便于入库/粘贴）", *selected),
                Line::from(""),
                choice_line(2, "取消", *selected),
                Line::from(""),
                Line::from(Span::styled(
                    "↑↓ 选择，Enter 确认，h/m 快捷选择，Esc 取消",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("导出")
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
                rect,
            );
        }
        ExportDialog::Path { kind, value, error } => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("导出 {}", kind.label()),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("输入导出路径（支持相对/绝对路径）："),
                Line::from(""),
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Cyan)),
                    Span::raw(value.clone()),
                    Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter 导出，Esc 返回，Backspace 删除，Delete 清空",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            if let Some(error) = error {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("失败：{error}"),
                    Style::default().fg(Color::Red),
                )));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("导出路径")
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
                rect,
            );
        }
    }
}

fn choice_line(index: usize, text: &str, selected: usize) -> Line<'static> {
    if index == selected {
        Line::from(Span::styled(
            format!("  ▸ {text}"),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(format!("    {text}"))
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn status_style(status: Status) -> Style {
    match status {
        Status::Unchanged => Style::default().fg(Color::DarkGray),
        Status::Added => Style::default().fg(Color::Green),
        Status::Removed => Style::default().fg(Color::Red),
        Status::Modified => Style::default().fg(Color::Yellow),
        Status::Moved => Style::default().fg(Color::Cyan),
        Status::SuspectedModified => Style::default().fg(Color::Magenta),
    }
}

fn record_preview(record: &Record, limit: usize) -> String {
    let text = record.display_text().replace(['\n', '\t'], " ");
    let mut preview: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_cycles_and_matches() {
        assert_eq!(Filter::All.cycle(1), Filter::Modified);
        assert_eq!(Filter::Modified.cycle(-1), Filter::All);
        assert_eq!(Filter::Moved.cycle(1), Filter::All);
        assert!(Filter::All.matches(Status::Unchanged));
        assert!(!Filter::Modified.matches(Status::SuspectedModified));
        assert_eq!(Filter::from_number('3'), Some(Filter::Suspected));
    }

    fn sample_record(
        id: usize,
        status: Status,
        old: Option<usize>,
        new: Option<usize>,
        old_text: Option<&str>,
        new_text: Option<&str>,
        order: f64,
    ) -> Record {
        Record {
            id,
            status,
            old_para_index: old,
            new_para_index: new,
            old_text: old_text.map(str::to_string),
            new_text: new_text.map(str::to_string),
            scores: crate::result::Scores::empty(),
            diff: status
                .is_modified_like()
                .then(|| crate::diff::para_diff(old_text.unwrap_or(""), new_text.unwrap_or(""))),
            row_diff: None,
            matched_to: new,
            note: None,
            table: None,
            order,
        }
    }

    fn sample_output(records: Vec<Record>) -> CompareOutput {
        let summary = crate::result::Summary::from_records(&records);
        CompareOutput {
            old_path: PathBuf::from("old.docx"),
            new_path: PathBuf::from("new.docx"),
            old_paragraphs: 4,
            new_paragraphs: 4,
            old_tables: 0,
            new_tables: 0,
            records,
            summary,
            semantic: crate::result::SemanticStatus::disabled("测试"),
            elapsed: Duration::from_millis(1),
        }
    }

    fn revision_output() -> CompareOutput {
        sample_output(vec![
            sample_record(
                1,
                Status::Unchanged,
                Some(1),
                Some(1),
                Some("甲段"),
                Some("甲段"),
                0.0,
            ),
            sample_record(
                2,
                Status::Removed,
                Some(2),
                None,
                Some("乙段旧文"),
                None,
                0.5,
            ),
            sample_record(
                3,
                Status::Modified,
                Some(3),
                Some(2),
                Some("公司每年检查"),
                Some("公司每季度检查"),
                1.0,
            ),
            sample_record(
                4,
                Status::Moved,
                Some(4),
                Some(3),
                Some("戊段"),
                Some("戊段"),
                2.0,
            ),
            sample_record(5, Status::Added, None, Some(4), None, Some("丁段新增"), 3.0),
        ])
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn v_switches_between_records_and_revision() {
        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output())));
        assert_eq!(app.view, ViewMode::Records);
        assert_eq!(app.revision.len(), 6);

        app.handle_key(key(KeyCode::Char('v')));
        assert_eq!(app.view, ViewMode::Revision);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.view, ViewMode::Records);
        app.handle_key(key(KeyCode::Char('v')));
        app.handle_key(key(KeyCode::Char('v')));
        assert_eq!(app.view, ViewMode::Records);
    }

    #[test]
    fn revision_view_marks_old_and_new_fragments() {
        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output())));
        let lines = revision_lines(&app.revision);
        let joined: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(joined.contains("移动自旧第 4 段"), "{joined}");
        assert!(joined.contains("移动至新第 3 段"), "{joined}");
        assert!(joined.contains("年"), "{joined}");
        assert!(joined.contains("季度"), "{joined}");

        let modified = lines
            .iter()
            .find(|line| line_text(line).contains("修改 旧3→新2"))
            .unwrap();
        let deleted = modified
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "年")
            .unwrap();
        let inserted = modified
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "季度")
            .unwrap();
        assert!(deleted.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(inserted.style.add_modifier.contains(Modifier::UNDERLINED));

        let removed = lines
            .iter()
            .find(|line| line_text(line).contains("- 删除 旧2"))
            .unwrap();
        assert!(
            removed
                .spans
                .iter()
                .find(|span| span.content.contains("乙段旧文"))
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
    }

    #[test]
    fn revision_scroll_keys_clamp_and_page() {
        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output())));
        app.handle_key(key(KeyCode::Char('v')));
        app.revision_max_scroll = 20;
        app.revision_page = 5;

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.revision_scroll, 1);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.revision_scroll, 6);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.revision_scroll, 20);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.revision_scroll, 20);
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.revision_scroll, 0);
    }

    #[test]
    fn revision_view_renders_and_caches_wrapped_height() {
        use ratatui::backend::TestBackend;

        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output())));
        app.handle_key(key(KeyCode::Char('v')));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                screen.push_str(buffer[(x, y)].symbol());
            }
            screen.push('\n');
        }
        let compact: String = screen.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(compact.contains("修订视图"), "{screen}");
        assert!(compact.contains("新增"), "{screen}");
        assert!(compact.contains("删除"), "{screen}");
        assert!(app.revision_metrics.is_some());

        app.revision_scroll = 999;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.revision_scroll <= app.revision_max_scroll);
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn table_summary_marks_whole_table() {
        let line = table_summary_line(&TableSummary {
            table: 2,
            caption: None,
            added: 0,
            removed: 1,
            modified: 0,
            moved: 0,
            whole: Some(Status::Removed),
        });
        let text = line_text(&line);
        assert!(text.contains("整表删除"), "{text}");
        assert!(!text.contains("删除 1 行"), "{text}");
    }

    #[test]
    fn detail_metrics_are_only_computed_when_detail_is_focused() {
        use ratatui::backend::TestBackend;

        let old = "公司应当每年开展一次安全检查。";
        let new = "公司应当每季度开展一次安全检查。";
        let output = CompareOutput {
            old_path: PathBuf::from("old.docx"),
            new_path: PathBuf::from("new.docx"),
            old_paragraphs: 1,
            new_paragraphs: 1,
            old_tables: 0,
            new_tables: 0,
            records: vec![Record {
                id: 1,
                status: Status::Modified,
                old_para_index: Some(1),
                new_para_index: Some(1),
                old_text: Some(old.to_string()),
                new_text: Some(new.to_string()),
                scores: crate::result::Scores {
                    text: 0.8,
                    semantic: None,
                    total: 0.8,
                },
                diff: Some(crate::diff::para_diff(old, new)),
                row_diff: None,
                matched_to: Some(1),
                note: None,
                table: None,
                order: 0.0,
            }],
            summary: crate::result::Summary {
                modified: 1,
                ..Default::default()
            },
            semantic: crate::result::SemanticStatus::disabled("测试"),
            elapsed: std::time::Duration::from_millis(1),
        };
        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.state = State::Ready(Box::new(output));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            app.detail_metrics.is_none(),
            "记录列表焦点下不应计算详情换行"
        );

        app.focus = Focus::Detail(DetailPane::Old);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.detail_metrics.is_some(), "进入详情后应缓存两栏换行行数");
    }
}
