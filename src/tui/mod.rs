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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::Config;
use crate::diff::excerpt;
use crate::engine;
use crate::engine::DocumentKind;
use crate::matching::Progress;
use crate::report;
use crate::result::{
    CompareOutput, DiffOp, DiffSpan, GridCell, GridCellKind, GridColumn, GridColumnKind, GridRow,
    GridRowKind, Record, Status, TableGrid,
};
use crate::revision::{self, RevisionBlock, RevisionKind, RevisionSpan};

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

    fn matches_row(self, kind: GridRowKind) -> bool {
        match self {
            Filter::All => true,
            Filter::Modified => kind == GridRowKind::Modified,
            Filter::Suspected => kind == GridRowKind::SuspectedModified,
            Filter::Added => kind == GridRowKind::Added,
            Filter::Removed => kind == GridRowKind::Removed,
            Filter::Moved => matches!(kind, GridRowKind::MovedFrom | GridRowKind::MovedTo),
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

    fn style(self) -> Style {
        match self {
            Filter::All => Style::default().fg(Color::Gray),
            Filter::Modified => Style::default().fg(Color::Yellow),
            Filter::Suspected => Style::default().fg(Color::Magenta),
            Filter::Added => Style::default().fg(Color::Green),
            Filter::Removed => Style::default().fg(Color::Red),
            Filter::Moved => Style::default().fg(Color::Cyan),
        }
        .add_modifier(Modifier::BOLD)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Records,
    Revision,
    Table,
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
    /// 修订视图里当前选中的表格序号（按修订流中的表格顺序）。
    revision_table_focus: usize,
    /// 每张表在修订视图中的视觉行号（按当前终端宽度换行后），用于 Tab 切换时定位。
    revision_table_lines: Vec<usize>,
    /// 进入表格修订视图时锁定的一张表。
    table_view: Option<TableGrid>,
    table_scroll: u16,
    table_max_scroll: u16,
    table_hscroll: u16,
    table_max_hscroll: u16,
    table_page: u16,
    /// 当前选中的数据行（0 起始，表头行不参与）。
    table_selected: usize,
    /// 当前筛选下可见的数据行在 `TableGrid.rows` 中的下标。
    table_visible: Vec<usize>,
    table_filter: Filter,
    /// 打开行详情浮层时记录的行号。
    table_detail: Option<usize>,
    table_detail_scroll: u16,
    table_detail_max: u16,
    table_detail_page: u16,
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
            revision_table_focus: 0,
            revision_table_lines: Vec::new(),
            table_view: None,
            table_scroll: 0,
            table_max_scroll: 0,
            table_hscroll: 0,
            table_max_hscroll: 0,
            table_page: 1,
            table_selected: 0,
            table_visible: Vec::new(),
            table_filter: Filter::All,
            table_detail: None,
            table_detail_scroll: 0,
            table_detail_max: 0,
            table_detail_page: 1,
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
                self.revision = revision::build(&output.records, &output.tables);
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
                self.revision_table_focus = 0;
                self.revision_table_lines.clear();
                self.table_view = None;
                self.table_filter = Filter::All;
                self.reset_table_navigation();
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
        if self.view == ViewMode::Table {
            self.handle_table_key(key);
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
            ViewMode::Table => ViewMode::Revision,
        };
        if self.view != ViewMode::Table {
            self.table_view = None;
            self.table_detail = None;
        }
    }

    fn revision_tables(&self) -> impl Iterator<Item = &TableGrid> {
        self.revision
            .iter()
            .filter_map(|block| block.table.as_ref())
    }

    fn revision_table_count(&self) -> usize {
        self.revision_tables().count()
    }

    fn cycle_revision_table(&mut self, forward: bool) {
        let count = self.revision_table_count();
        if count == 0 {
            return;
        }
        let current = self.revision_table_focus.min(count - 1);
        self.revision_table_focus = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.scroll_to_revision_table();
    }

    fn scroll_to_revision_table(&mut self) {
        let Some(line) = self
            .revision_table_lines
            .get(self.revision_table_focus)
            .copied()
        else {
            return;
        };
        let page = self.revision_page.max(1) as usize;
        let target = line.saturating_sub(page / 3);
        self.revision_scroll = target.min(self.revision_max_scroll as usize) as u16;
    }

    fn reset_table_navigation(&mut self) {
        self.table_scroll = 0;
        self.table_max_scroll = 0;
        self.table_hscroll = 0;
        self.table_max_hscroll = 0;
        self.table_selected = 0;
        self.table_detail = None;
        self.table_detail_scroll = 0;
        self.table_detail_max = 0;
        self.refresh_table_visible();
    }

    fn refresh_table_visible(&mut self) {
        self.table_visible = self
            .table_view
            .as_ref()
            .map(|table| visible_table_rows(table, self.table_filter))
            .unwrap_or_default();
        if self.table_visible.is_empty() {
            self.table_selected = 0;
        } else {
            self.table_selected = self.table_selected.min(self.table_visible.len() - 1);
        }
    }

    fn set_table_filter(&mut self, filter: Filter) {
        self.table_filter = filter;
        self.table_selected = 0;
        self.table_scroll = 0;
        self.table_detail = None;
        self.refresh_table_visible();
        self.scroll_table_to_selected();
    }

    fn open_table_view(&mut self) {
        let Some(table) = self
            .revision_tables()
            .nth(self.revision_table_focus)
            .cloned()
        else {
            return;
        };
        self.table_view = Some(table);
        self.reset_table_navigation();
        self.view = ViewMode::Table;
    }

    fn switch_table_view(&mut self, forward: bool) {
        let count = self.revision_table_count();
        if count == 0 {
            return;
        }
        let current = self.revision_table_focus.min(count - 1);
        let next = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.revision_table_focus = next;
        let next_table = self.revision_tables().nth(next).cloned();
        if let Some(table) = next_table {
            self.table_view = Some(table);
            self.reset_table_navigation();
        }
    }

    fn scroll_revision(&mut self, delta: isize) {
        let current = self.revision_scroll as isize;
        let max = self.revision_max_scroll as isize;
        self.revision_scroll = current.saturating_add(delta).clamp(0, max) as u16;
    }

    fn scroll_table_h(&mut self, delta: isize) {
        let current = self.table_hscroll as isize;
        let max = self.table_max_hscroll as isize;
        self.table_hscroll = current.saturating_add(delta).clamp(0, max) as u16;
    }

    fn handle_revision_key(&mut self, key: KeyEvent) {
        let page = self.revision_page.max(1) as isize;
        let half = (page / 2).max(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.view = ViewMode::Records,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('e') => self.open_export(),
            KeyCode::Tab => self.cycle_revision_table(true),
            KeyCode::BackTab => self.cycle_revision_table(false),
            KeyCode::Enter => self.open_table_view(),
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

    fn table_row_count(&self) -> usize {
        self.table_visible.len()
    }

    fn move_table_selection(&mut self, delta: isize) {
        let count = self.table_row_count();
        if count == 0 {
            return;
        }
        let selected = (self.table_selected as isize)
            .saturating_add(delta)
            .clamp(0, count as isize - 1) as usize;
        self.table_selected = selected;
        self.scroll_table_to_selected();
    }

    fn select_table_row(&mut self, index: usize) {
        let count = self.table_row_count();
        if count == 0 {
            return;
        }
        self.table_selected = index.min(count - 1);
        self.scroll_table_to_selected();
    }

    fn scroll_table_to_selected(&mut self) {
        let count = self.table_row_count();
        if count == 0 {
            return;
        }
        let line = 3 + self.table_selected.min(count - 1);
        let page = self.table_page.max(1) as usize;
        let scroll = self.table_scroll as usize;
        let target = if line < scroll {
            line
        } else if line >= scroll + page {
            line + 1 - page
        } else {
            scroll
        };
        self.table_scroll = target.min(self.table_max_scroll as usize) as u16;
    }

    fn open_table_detail(&mut self) {
        if self.table_row_count() == 0 {
            return;
        }
        let Some(&row_index) = self.table_visible.get(self.table_selected) else {
            return;
        };
        self.table_detail = Some(row_index);
        self.table_detail_scroll = 0;
        self.table_detail_max = 0;
    }

    fn scroll_table_detail(&mut self, delta: isize) {
        let current = self.table_detail_scroll as isize;
        let max = self.table_detail_max as isize;
        self.table_detail_scroll = current.saturating_add(delta).clamp(0, max) as u16;
    }

    fn handle_table_detail_key(&mut self, key: KeyEvent) {
        let page = self.table_detail_page.max(1) as isize;
        let half = (page / 2).max(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.table_detail = None,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_table_detail(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_table_detail(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_table_detail(-page),
            KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char(' ') => {
                self.scroll_table_detail(page);
            }
            KeyCode::Char('u') => self.scroll_table_detail(-half),
            KeyCode::Char('d') => self.scroll_table_detail(half),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_table_detail(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.scroll_table_detail(isize::MAX / 2),
            _ => {}
        }
    }

    fn handle_table_key(&mut self, key: KeyEvent) {
        if self.table_detail.is_some() {
            self.handle_table_detail_key(key);
            return;
        }
        let page = self.table_page.max(1) as isize;
        let half = (page / 2).max(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                self.view = ViewMode::Revision;
                self.table_view = None;
                self.table_detail = None;
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('e') => self.open_export(),
            KeyCode::Enter => self.open_table_detail(),
            KeyCode::Tab => self.set_table_filter(self.table_filter.cycle(1)),
            KeyCode::BackTab => self.set_table_filter(self.table_filter.cycle(-1)),
            KeyCode::Char(ch @ '1'..='6') => {
                if let Some(filter) = Filter::from_number(ch) {
                    self.set_table_filter(filter);
                }
            }
            KeyCode::Char('[') => self.switch_table_view(false),
            KeyCode::Char(']') => self.switch_table_view(true),
            KeyCode::Left | KeyCode::Char('h') => self.scroll_table_h(-4),
            KeyCode::Right | KeyCode::Char('l') => self.scroll_table_h(4),
            KeyCode::Up | KeyCode::Char('k') => self.move_table_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_table_selection(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.move_table_selection(-page),
            KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char(' ') => {
                self.move_table_selection(page);
            }
            KeyCode::Char('u') => self.move_table_selection(-half),
            KeyCode::Char('d') => self.move_table_selection(half),
            KeyCode::Home | KeyCode::Char('g') => self.select_table_row(0),
            KeyCode::End | KeyCode::Char('G') => {
                let count = self.table_row_count();
                if count > 0 {
                    self.select_table_row(count - 1);
                }
            }
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
    if app.view == ViewMode::Table {
        draw_table_view(frame, layout[1], app);
        draw_table_bar(frame, layout[0], app);
        return;
    }
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

fn draw_table_bar(frame: &mut Frame, area: Rect, app: &App) {
    let Some(table) = app.table_view.as_ref() else {
        return;
    };
    let total = app.revision_table_count();
    let position = if total == 0 {
        String::new()
    } else {
        format!(
            "  表格 {}/{}",
            (app.revision_table_focus + 1).min(total),
            total
        )
    };
    let mut spans = vec![
        Span::styled(
            "表格修订",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", table_label(table)),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(position, Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("  筛选：{}", app.table_filter.label()),
            app.table_filter.style(),
        ),
    ];
    let rows = app.table_visible.len();
    if rows > 0 {
        spans.push(Span::styled(
            format!("  行 {}/{}", app.table_selected.min(rows - 1) + 1, rows),
            Style::default().fg(Color::Cyan),
        ));
    } else {
        spans.push(Span::styled(
            "  行 0/0",
            Style::default().fg(Color::DarkGray),
        ));
    }
    if app.table_max_hscroll > 0 {
        spans.push(Span::styled(
            format!(
                "  横向 {}/{}",
                app.table_hscroll.min(app.table_max_hscroll),
                app.table_max_hscroll
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let bar = Layout::horizontal([Constraint::Min(0), Constraint::Length(18)]).split(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), bar[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "v/Esc 返回修订",
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Right),
        bar[1],
    );
}

fn draw_table_view(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(vec![
            Span::styled(
                "表格修订",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "：完整行列（删除线=旧值，下划线=新值）",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    app.refresh_table_visible();
    let Some(table) = app.table_view.as_ref() else {
        return;
    };
    let row_count = app.table_visible.len();
    let (mut lines, max_hscroll) =
        table_view_lines(table, inner.width, app.table_hscroll, &app.table_visible);
    let hscroll = app.table_hscroll.min(max_hscroll);
    app.table_hscroll = hscroll;
    app.table_max_hscroll = max_hscroll;
    if row_count == 0 {
        app.table_selected = 0;
    } else {
        app.table_selected = app.table_selected.min(row_count - 1);
        let index = 3 + app.table_selected;
        if let Some(line) = lines.get(index) {
            let used: usize = line
                .spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            let mut spans = line.spans.clone();
            if used < inner.width as usize {
                spans.push(Span::raw(" ".repeat(inner.width as usize - used)));
            }
            lines[index] = Line::from(spans).style(Style::default().bg(Color::Rgb(38, 48, 68)));
        }
    }
    let max = lines
        .len()
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    let scroll = app.table_scroll.min(max);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
    app.table_scroll = scroll;
    app.table_max_scroll = max;
    app.table_page = inner.height;
    if let Some(row_index) = app.table_detail {
        draw_table_detail(frame, area, app, row_index);
    }
}

fn draw_table_detail(frame: &mut Frame, area: Rect, app: &mut App, row_index: usize) {
    let Some(table) = app.table_view.as_ref() else {
        return;
    };
    if table.rows.get(row_index).is_none() {
        app.table_detail = None;
        return;
    }
    let rect = centered_rect(84, 76, area);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(vec![
            Span::styled(
                "行详情",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {}", table_label(table)),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    let inner = block.inner(rect);
    let lines = table_detail_lines(table, row_index);
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width);
    let max = total
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    let scroll = app.table_detail_scroll.min(max);
    frame.render_widget(paragraph.scroll((scroll, 0)), rect);
    app.table_detail_scroll = scroll;
    app.table_detail_max = max;
    app.table_detail_page = inner.height;
}

fn draw_revision(frame: &mut Frame, area: Rect, app: &mut App) {
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

    let table_count = app.revision_table_count();
    if table_count == 0 {
        app.revision_table_focus = 0;
    } else if app.revision_table_focus >= table_count {
        app.revision_table_focus = table_count - 1;
    }
    let focus = (table_count > 0).then_some(app.revision_table_focus);
    let (lines, table_lines) = revision_lines(&app.revision, inner.width, focus);
    app.revision_table_lines = table_lines;
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

fn revision_lines(
    blocks: &[RevisionBlock],
    width: u16,
    table_focus: Option<usize>,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut table_lines = Vec::new();
    let mut table_index = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        if block.table.is_some() {
            table_lines.push(lines.len());
            let focused = table_focus == Some(table_index);
            table_index += 1;
            if let Some(table) = block.table.as_ref() {
                lines.push(table_placeholder_line(table, focused, width));
            }
            continue;
        }
        append_revision_block(&mut lines, block);
    }
    let mut table_positions = Vec::with_capacity(table_lines.len());
    let mut next = 0usize;
    let mut offset = 0usize;
    for (index, line) in lines.iter().enumerate() {
        while next < table_lines.len() && table_lines[next] == index {
            table_positions.push(offset);
            next += 1;
        }
        offset += line_visual_height(line, width);
    }
    (lines, table_positions)
}

fn line_visual_height(line: &Line<'_>, width: u16) -> usize {
    let total: usize = line
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let width = width.max(1) as usize;
    total.div_ceil(width).max(1)
}

fn append_revision_block(lines: &mut Vec<Line<'static>>, block: &RevisionBlock) {
    let (tag, style) = revision_tag(block);
    let mut current = Line::from(vec![Span::styled(tag, style)]);
    if let Some(note) = block.note.as_ref() {
        current.spans.push(Span::styled(
            format!("（{note}）"),
            Style::default().fg(Color::Cyan),
        ));
    }
    current.spans.push(Span::raw(" "));
    extend_revision_spans(lines, &mut current, &block.spans);
    lines.push(current);
}

fn extend_revision_spans(
    lines: &mut Vec<Line<'static>>,
    current: &mut Line<'static>,
    spans: &[RevisionSpan],
) {
    for span in spans {
        let span_style = revision_span_style(span.op);
        for (index, part) in span.text.split('\n').enumerate() {
            if index > 0 {
                lines.push(std::mem::replace(current, Line::from("")));
            }
            if !part.is_empty() {
                current
                    .spans
                    .push(Span::styled(part.to_string(), span_style));
            }
        }
    }
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

fn table_label(table: &TableGrid) -> String {
    match table.caption.as_ref() {
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
    }
}

fn table_placeholder_line(table: &TableGrid, focused: bool, width: u16) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            if focused { "> " } else { "  " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            table_label(table),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            table_summary_text(table),
            Style::default().fg(if focused {
                Color::White
            } else {
                Color::DarkGray
            }),
        ),
    ];
    if focused {
        spans.push(Span::styled(
            "   Enter 查看表格修订",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let used: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let effective = width.saturating_sub(GRID_SAFETY as u16);
    let mut line = clip_line(Line::from(spans), effective);
    if focused {
        let filled = used.min(effective as usize);
        if filled < effective as usize {
            line.spans
                .push(Span::raw(" ".repeat(effective as usize - filled)));
        }
        line = line.style(Style::default().bg(Color::Rgb(38, 48, 68)));
    }
    line
}

fn table_view_lines(
    table: &TableGrid,
    width: u16,
    hscroll: u16,
    visible: &[usize],
) -> (Vec<Line<'static>>, u16) {
    let mut lines = Vec::new();
    if table.columns.is_empty() {
        lines.push(clip_line(
            Line::from(Span::styled(
                "     （表格无列信息）",
                Style::default().fg(Color::DarkGray),
            )),
            width,
        ));
        return (lines, 0);
    }
    let widths = grid_widths(table);
    let view = width.saturating_sub(GRID_SAFETY as u16) as usize;
    let cell_area = grid_total_width(&widths).saturating_sub(GRID_GUTTER);
    let max_hscroll = cell_area.saturating_sub(view.saturating_sub(GRID_GUTTER));
    let offset = (hscroll as usize).min(max_hscroll);
    lines.push(clip_line(table_legend_line(table), width));
    lines.push(window_line(table_header_line(table, &widths), offset, view));
    lines.push(window_line(table_separator_line(&widths), offset, view));
    for &row_index in visible {
        if let Some(row) = table.rows.get(row_index) {
            lines.push(window_line(table_row_line(row, &widths), offset, view));
        }
    }
    if visible.is_empty() {
        let message = if table.rows.is_empty() {
            "（表格无数据行）"
        } else {
            "（当前筛选无变化行）"
        };
        lines.push(clip_line(
            Line::from(Span::styled(
                format!("      {message}"),
                Style::default().fg(Color::DarkGray),
            )),
            width,
        ));
    }
    lines.push(window_line(table_separator_line(&widths), offset, view));
    (lines, max_hscroll.min(u16::MAX as usize) as u16)
}

/// 保留左侧行号/标记栏，只对列区域做横向开窗。
fn window_line(line: Line<'static>, offset: usize, view: usize) -> Line<'static> {
    if view == 0 {
        return Line::from("");
    }
    if view <= GRID_GUTTER {
        return clip_line(line, view as u16);
    }
    let total: usize = line
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if offset == 0 && total <= view {
        return line;
    }
    let start = GRID_GUTTER + offset;
    let end = start + (view - GRID_GUTTER);
    let mut spans = Vec::new();
    let mut col = 0usize;
    let mut kept = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut text = String::new();
        for ch in span.content.chars() {
            let ch_width = if ch == '\n' || ch == '\t' {
                1
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            };
            let from = col;
            col += ch_width;
            if col <= GRID_GUTTER || (from >= start && col <= end) {
                text.push(if ch == '\n' || ch == '\t' { ' ' } else { ch });
                kept += ch_width;
            } else if from < start && col > start && from >= GRID_GUTTER {
                let pad = col - start;
                text.push_str(&" ".repeat(pad));
                kept += pad;
            }
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
    }
    if kept < view {
        spans.push(Span::raw(" ".repeat(view - kept)));
    }
    Line::from(spans)
}

fn clip_line(line: Line<'static>, width: u16) -> Line<'static> {
    let max = width as usize;
    let total: usize = line
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if total <= max {
        return line;
    }
    let limit = max.saturating_sub(1);
    let mut spans = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for span in line.spans {
        let style = span.style;
        let mut text = String::new();
        for ch in span.content.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + char_width > limit {
                truncated = true;
                break;
            }
            text.push(ch);
            used += char_width;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
        if truncated {
            break;
        }
    }
    spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
    Line::from(spans)
}

fn visible_table_rows(table: &TableGrid, filter: Filter) -> Vec<usize> {
    table
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| filter.matches_row(row.kind))
        .map(|(index, _)| index)
        .collect()
}

fn grid_has_changes(table: &TableGrid) -> bool {
    table.whole.is_some()
        || table.summary.differences() > 0
        || table.columns.iter().any(|column| {
            column.kind != GridColumnKind::Unchanged
                || column
                    .old_label
                    .as_deref()
                    .is_some_and(|old| old != column.label)
        })
}

fn table_summary_text(table: &TableGrid) -> String {
    if let Some(whole) = table.whole {
        return format!(
            "{} | 共 {} 行",
            match whole {
                Status::Added => "整表新增",
                Status::Removed => "整表删除",
                _ => "整表变化",
            },
            table.rows.len()
        );
    }
    if !grid_has_changes(table) {
        return "无变化".to_string();
    }
    let summary = &table.summary;
    let mut parts = vec![format!("未变 {}", summary.unchanged)];
    if summary.added > 0 {
        parts.push(format!("新增 {}", summary.added));
    }
    if summary.removed > 0 {
        parts.push(format!("删除 {}", summary.removed));
    }
    if summary.modified > 0 {
        parts.push(format!("修改 {}", summary.modified));
    }
    if summary.suspected_modified > 0 {
        parts.push(format!("疑似 {}", summary.suspected_modified));
    }
    if summary.moved > 0 {
        parts.push(format!("移动 {}", summary.moved));
    }
    let renamed = table
        .columns
        .iter()
        .filter(|column| {
            column.kind == GridColumnKind::Unchanged
                && column
                    .old_label
                    .as_deref()
                    .is_some_and(|old| old != column.label)
        })
        .count();
    if renamed > 0 {
        parts.push(format!("改列名 {renamed}"));
    }
    parts.join(" | ")
}

fn table_legend_line(table: &TableGrid) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "      标记：",
        Style::default().fg(Color::DarkGray),
    )];
    spans.push(Span::styled("+ 新增  ", Style::default().fg(Color::Green)));
    spans.push(Span::styled("- 删除  ", Style::default().fg(Color::Red)));
    spans.push(Span::styled("~ 修改  ", Style::default().fg(Color::Yellow)));
    spans.push(Span::styled("> 移入  ", Style::default().fg(Color::Green)));
    spans.push(Span::styled("< 移出", Style::default().fg(Color::Red)));
    let moves: Vec<String> = table
        .rows
        .iter()
        .filter(|row| row.kind == GridRowKind::MovedTo)
        .filter_map(|row| Some(format!("旧{}->新{}", row.old_row?, row.new_row?)))
        .collect();
    if !moves.is_empty() {
        spans.push(Span::styled(
            format!("  （移动：{}）", moves.join("，")),
            Style::default().fg(Color::Cyan),
        ));
    }
    let reordered: Vec<String> = table
        .rows
        .iter()
        .filter(|row| row.note.as_deref() == Some("行序变化"))
        .filter_map(|row| Some(format!("旧{}->新{}", row.old_row?, row.new_row?)))
        .collect();
    if !reordered.is_empty() {
        spans.push(Span::styled(
            format!("  （行序变化：{}）", reordered.join("，")),
            Style::default().fg(Color::Yellow),
        ));
    }
    Line::from(spans)
}

const GRID_GUTTER: usize = 6;
const GRID_MIN_COL: usize = 6;
const GRID_MAX_COL: usize = 36;
/// 右侧留 2 列余量：个别字符（如破折号）在部分终端按双宽渲染时，
/// 不至于把整行推到窗口宽度之外而折行。
const GRID_SAFETY: usize = 2;

fn grid_total_width(widths: &[usize]) -> usize {
    GRID_GUTTER + widths.len() * 3 + 1 + widths.iter().sum::<usize>()
}

/// 各列取内容自然宽度并夹在最小/最大列宽之间；超出终端时靠横向滚动查看，
/// 不再把列压到只剩省略号。
fn grid_widths(table: &TableGrid) -> Vec<usize> {
    table
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let mut max = UnicodeWidthStr::width(column_header_text(column).as_str());
            for row in &table.rows {
                if let Some(cell) = row.cells.get(index) {
                    let spans = grid_cell_spans(row, cell);
                    let text: String = spans.iter().map(|span| span.text.as_str()).collect();
                    max = max.max(UnicodeWidthStr::width(text.as_str()));
                }
            }
            max.clamp(GRID_MIN_COL, GRID_MAX_COL)
        })
        .collect()
}

fn column_label_style(kind: GridColumnKind) -> Style {
    match kind {
        GridColumnKind::Added => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        GridColumnKind::Removed => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        GridColumnKind::Unchanged => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    }
}

fn table_header_line(table: &TableGrid, widths: &[usize]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(GRID_GUTTER))];
    for (column, width) in table.columns.iter().zip(widths) {
        spans.push(Span::styled("|", grid_bar_style()));
        spans.push(Span::raw(" "));
        spans.extend(fit_text_with_style(
            &column_header_text(column),
            *width,
            column_label_style(column.kind),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled("|", grid_bar_style()));
    Line::from(spans)
}

fn table_separator_line(widths: &[usize]) -> Line<'static> {
    let mut spans = vec![Span::styled("-".repeat(GRID_GUTTER), grid_bar_style())];
    for width in widths {
        spans.push(Span::styled("+", grid_bar_style()));
        spans.push(Span::styled("-".repeat(width + 2), grid_bar_style()));
    }
    spans.push(Span::styled("+", grid_bar_style()));
    Line::from(spans)
}

fn table_row_line(row: &GridRow, widths: &[usize]) -> Line<'static> {
    let (marker, style) = grid_row_marker(row.kind);
    let number = match row.kind {
        GridRowKind::Removed | GridRowKind::MovedFrom => row.old_row,
        _ => row.new_row.or(row.old_row),
    };
    let number = number
        .map(|value| format!("{value:>3}"))
        .unwrap_or_else(|| "  -".to_string());
    let mut spans = vec![
        Span::styled(marker, style),
        Span::raw(" "),
        Span::styled(number, Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
    ];
    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::styled("|", grid_bar_style()));
        spans.push(Span::raw(" "));
        let cell_spans = row
            .cells
            .get(index)
            .map(|cell| grid_cell_spans(row, cell))
            .unwrap_or_default();
        spans.extend(clip_spans(&cell_spans, *width));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled("|", grid_bar_style()));
    Line::from(spans)
}

fn row_position_label(row: &GridRow) -> String {
    match (row.old_row, row.new_row) {
        (Some(old), Some(new)) if old == new => format!("第 {new} 行"),
        (Some(old), Some(new)) => format!("旧 #{old} → 新 #{new}"),
        (Some(old), None) => format!("旧 #{old}（已删除）"),
        (None, Some(new)) => format!("新 #{new}（新增）"),
        (None, None) => "行号未知".to_string(),
    }
}

fn row_kind_label(kind: GridRowKind) -> &'static str {
    match kind {
        GridRowKind::Unchanged => "未变",
        GridRowKind::Added => "新增",
        GridRowKind::Removed => "删除",
        GridRowKind::Modified => "修改",
        GridRowKind::SuspectedModified => "疑似修改",
        GridRowKind::MovedFrom => "移出",
        GridRowKind::MovedTo => "移入",
    }
}

fn table_detail_lines(table: &TableGrid, row_index: usize) -> Vec<Line<'static>> {
    let Some(row) = table.rows.get(row_index) else {
        return Vec::new();
    };
    let (marker, style) = grid_row_marker(row.kind);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(
            format!("{} · {}", row_kind_label(row.kind), row_position_label(row)),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    if let Some(note) = row.note.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("  {note}"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    for (index, column) in table.columns.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            column_header_text(column),
            column_label_style(column.kind),
        )));
        lines.extend(detail_cell_lines(row, row.cells.get(index)));
    }
    lines
}

fn detail_cell_lines(row: &GridRow, cell: Option<&GridCell>) -> Vec<Line<'static>> {
    let Some(cell) = cell else {
        return vec![detail_value_line("（无此列）", None, DiffOp::Equal)];
    };
    let row_op = match row.kind {
        GridRowKind::Added | GridRowKind::MovedTo => Some(DiffOp::Insert),
        GridRowKind::Removed | GridRowKind::MovedFrom => Some(DiffOp::Delete),
        _ => None,
    };
    if let Some(op) = row_op {
        let text = if op == DiffOp::Insert {
            cell.new.as_deref().or(cell.old.as_deref())
        } else {
            cell.old.as_deref().or(cell.new.as_deref())
        };
        return vec![detail_value_line("", text, op)];
    }
    match cell.kind {
        GridCellKind::Unchanged => vec![detail_value_line(
            "",
            cell.new.as_deref().or(cell.old.as_deref()),
            DiffOp::Equal,
        )],
        GridCellKind::Added => vec![detail_value_line(
            "新增：",
            cell.new.as_deref(),
            DiffOp::Insert,
        )],
        GridCellKind::Removed => {
            vec![detail_value_line(
                "删除：",
                cell.old.as_deref(),
                DiffOp::Delete,
            )]
        }
        GridCellKind::Modified => {
            let spans = match cell.diff.as_ref() {
                Some(diff) => sanitize_spans(revision::merge_diff(diff)),
                None => {
                    let mut spans = text_spans(cell.old.as_deref(), DiffOp::Delete);
                    spans.extend(text_spans(cell.new.as_deref(), DiffOp::Insert));
                    spans
                }
            };
            vec![detail_span_line("差异：", &spans)]
        }
    }
}

fn detail_span_line(prefix: &str, spans: &[RevisionSpan]) -> Line<'static> {
    let mut out = vec![Span::raw("  ")];
    if !prefix.is_empty() {
        out.push(Span::styled(
            prefix.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if spans.is_empty() {
        out.push(Span::styled("（空）", Style::default().fg(Color::DarkGray)));
    } else {
        for span in spans {
            out.push(Span::styled(
                span.text.clone(),
                revision_span_style(span.op),
            ));
        }
    }
    Line::from(out)
}

fn detail_value_line(prefix: &str, text: Option<&str>, op: DiffOp) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    if !prefix.is_empty() {
        spans.push(Span::styled(
            prefix.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let text = text
        .map(sanitize_cell_text)
        .filter(|value| !value.is_empty());
    match text {
        Some(text) => spans.push(Span::styled(text, revision_span_style(op))),
        None => spans.push(Span::styled("（空）", Style::default().fg(Color::DarkGray))),
    }
    Line::from(spans)
}

fn column_header_text(column: &GridColumn) -> String {
    let label = sanitize_cell_text(&column.label);
    match column.kind {
        GridColumnKind::Added => format!("+{label}"),
        GridColumnKind::Removed => format!("-{label}"),
        GridColumnKind::Unchanged => match column.old_label.as_deref() {
            Some(old) if old != column.label => {
                format!("{}->{label}", sanitize_cell_text(old))
            }
            _ => label,
        },
    }
}

fn grid_row_marker(kind: GridRowKind) -> (&'static str, Style) {
    match kind {
        GridRowKind::Unchanged => (" ", Style::default()),
        GridRowKind::Added => (
            "+",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        GridRowKind::Removed => (
            "-",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        GridRowKind::Modified => (
            "~",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        GridRowKind::SuspectedModified => (
            "~",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        GridRowKind::MovedFrom => (
            "<",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        GridRowKind::MovedTo => (
            ">",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    }
}

fn grid_cell_spans(row: &GridRow, cell: &GridCell) -> Vec<RevisionSpan> {
    let row_op = match row.kind {
        GridRowKind::Added | GridRowKind::MovedTo => Some(DiffOp::Insert),
        GridRowKind::Removed | GridRowKind::MovedFrom => Some(DiffOp::Delete),
        _ => None,
    };
    if let Some(op) = row_op {
        let text = if op == DiffOp::Insert {
            cell.new.as_deref().or(cell.old.as_deref())
        } else {
            cell.old.as_deref().or(cell.new.as_deref())
        };
        return text_spans(text, op);
    }
    match cell.kind {
        GridCellKind::Unchanged => {
            text_spans(cell.new.as_deref().or(cell.old.as_deref()), DiffOp::Equal)
        }
        GridCellKind::Added => text_spans(cell.new.as_deref(), DiffOp::Insert),
        GridCellKind::Removed => text_spans(cell.old.as_deref(), DiffOp::Delete),
        GridCellKind::Modified => {
            if let Some(diff) = cell.diff.as_ref() {
                sanitize_spans(revision::merge_diff(diff))
            } else {
                let mut spans = text_spans(cell.old.as_deref(), DiffOp::Delete);
                spans.extend(text_spans(cell.new.as_deref(), DiffOp::Insert));
                spans
            }
        }
    }
}

fn sanitize_cell_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn sanitize_spans(spans: Vec<RevisionSpan>) -> Vec<RevisionSpan> {
    spans
        .into_iter()
        .map(|span| RevisionSpan {
            op: span.op,
            text: sanitize_cell_text(&span.text),
        })
        .collect()
}

fn text_spans(text: Option<&str>, op: DiffOp) -> Vec<RevisionSpan> {
    text.filter(|text| !text.is_empty())
        .map(|text| {
            vec![RevisionSpan {
                op,
                text: sanitize_cell_text(text),
            }]
        })
        .unwrap_or_default()
}

fn clip_spans(spans: &[RevisionSpan], width: usize) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let total: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.text.as_str()))
        .sum();
    let limit = if total <= width {
        width
    } else {
        width.saturating_sub(1)
    };
    let mut result = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for span in spans {
        let style = revision_span_style(span.op);
        let mut text = String::new();
        for ch in span.text.chars() {
            let ch_width = if ch == '\n' || ch == '\t' {
                1
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            };
            if used + ch_width > limit {
                truncated = true;
                break;
            }
            text.push(if ch == '\n' || ch == '\t' { ' ' } else { ch });
            used += ch_width;
        }
        if !text.is_empty() {
            result.push(Span::styled(text, style));
        }
        if truncated {
            break;
        }
    }
    if truncated {
        result.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
        used += 1;
    }
    if used < width {
        result.push(Span::raw(" ".repeat(width - used)));
    }
    result
}

fn fit_text_with_style(text: &str, width: usize, style: Style) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let total = UnicodeWidthStr::width(text);
    let limit = if total <= width {
        width
    } else {
        width.saturating_sub(1)
    };
    let mut output = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = if ch == '\n' || ch == '\t' {
            1
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if used + ch_width > limit {
            break;
        }
        output.push(if ch == '\n' || ch == '\t' { ' ' } else { ch });
        used += ch_width;
    }
    let mut result = Vec::new();
    if !output.is_empty() {
        result.push(Span::styled(output, style));
    }
    if total > width {
        result.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
        used += 1;
    }
    if used < width {
        result.push(Span::raw(" ".repeat(width - used)));
    }
    result
}

fn grid_bar_style() -> Style {
    Style::default().fg(Color::DarkGray)
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
        State::Ready(_) if app.view == ViewMode::Table => {
            if app.table_detail.is_some() {
                "焦点：行详情   ↑↓/jk 滚动   PgUp/PgDn 翻页   d/u 半页   g/G 首尾   q/Esc/Enter 关闭   ? 帮助   Ctrl+C 退出"
                    .to_string()
            } else {
                "焦点：表格修订   Tab 筛选   ↑↓/jk 选行   ←→/hl 横向   Enter 行详情   [ ] 切表   v/Esc 返回修订   ? 帮助   q 退出"
                    .to_string()
            }
        }
        State::Ready(_) if app.view == ViewMode::Revision => {
            let tables = app.revision_table_count();
            if tables == 0 {
                "焦点：修订视图   j/k 滚动   PgUp/PgDn 翻页   d/u 半页   g/G 首尾   v/Esc 返回记录   e 导出   ? 帮助   q 退出"
                    .to_string()
            } else {
                format!(
                    "焦点：修订视图   表格 {}/{}   Tab 选表   Enter 表格修订   j/k 滚动   PgUp/PgDn 翻页   v/Esc 返回记录   ? 帮助   q 退出",
                    (app.revision_table_focus + 1).min(tables),
                    tables
                )
            }
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
        Line::from("移动段新位置注明“移动自旧第 N 段”；表格显示为占位行，不展开"),
        Line::from("Tab/Shift+Tab 选择表格   Enter 进入表格修订视图"),
        Line::from(""),
        Line::from(Span::styled("表格修订", Style::default().fg(Color::Yellow))),
        Line::from("整表按对齐后的行列显示；筛选为“全部”时未变行也保留"),
        Line::from("行首 + 新增 / - 删除 / ~ 修改 / > 移入 / < 移出；移动行在新旧位置各出现一次"),
        Line::from("列有最小宽度，超宽时用 h/l 或 ←/→ 横向滚动（行号栏固定）"),
        Line::from("Tab/Shift+Tab 循环筛选变更类型，1-6 直选（全部/修改/疑似/新增/删除/移动）"),
        Line::from("↑↓/jk 选择行（高亮）   Enter 打开行详情，完整查看该行内容"),
        Line::from("行详情：修改格按修订模式合并显示（删除线=旧片段，下划线=新片段）"),
        Line::from("行详情：↑↓/jk 滚动   PgUp/PgDn 翻页   q/Esc/Enter 关闭"),
        Line::from("[ / ] 切换表格   PgUp/PgDn 翻页选行   d/u 半页   g/G 首尾   v/Esc 返回修订"),
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
            tables: Vec::new(),
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

    fn revision_output_with_table() -> CompareOutput {
        let mut output = revision_output();
        let mut grid = sample_grid();
        grid.order = 1.5;
        output.tables.push(grid);
        output.old_tables = 1;
        output.new_tables = 1;
        output
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
        let lines = revision_lines(&app.revision, 100, None).0;
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
    fn table_horizontal_scroll_keys_clamp_and_reset() {
        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output_with_table())));
        app.view = ViewMode::Table;
        app.table_view = Some(sample_grid());
        app.table_max_hscroll = 20;

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.table_hscroll, 4);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.table_hscroll, 8);
        for _ in 0..10 {
            app.handle_key(key(KeyCode::Char('l')));
        }
        assert_eq!(app.table_hscroll, 20);
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.table_hscroll, 16);

        app.open_table_view();
        assert_eq!(app.table_hscroll, 0, "重新进入表格视图应回到最左");
        assert_eq!(app.table_max_hscroll, 0);
    }

    fn long_detail_table() -> TableGrid {
        TableGrid {
            table: 1,
            caption: Some("整改台账".to_string()),
            order: 0.0,
            columns: vec![
                column("事项", GridColumnKind::Unchanged),
                column("整改措施", GridColumnKind::Unchanged),
            ],
            rows: vec![
                GridRow {
                    kind: GridRowKind::Modified,
                    old_row: Some(30),
                    new_row: Some(30),
                    cells: vec![
                        equal_cell("第30项"),
                        changed_cell(
                            "继续跟踪第30项，确保按期完成并每周报送进度",
                            "继续跟踪第30项，确保按期完成，每周报送进度并附现场照片",
                        ),
                    ],
                    note: None,
                },
                GridRow {
                    kind: GridRowKind::Unchanged,
                    old_row: Some(31),
                    new_row: Some(31),
                    cells: vec![equal_cell("第31项"), equal_cell("已完成")],
                    note: None,
                },
            ],
            summary: crate::result::Summary {
                unchanged: 1,
                modified: 1,
                ..Default::default()
            },
            whole: None,
        }
    }

    fn filter_sample_table() -> TableGrid {
        let mut table = sample_grid();
        table.columns = vec![
            column("事项", GridColumnKind::Unchanged),
            column("值", GridColumnKind::Unchanged),
        ];
        table.rows = vec![
            GridRow {
                kind: GridRowKind::Unchanged,
                old_row: Some(2),
                new_row: Some(2),
                cells: vec![equal_cell("未变行"), equal_cell("0")],
                note: None,
            },
            GridRow {
                kind: GridRowKind::Modified,
                old_row: Some(3),
                new_row: Some(3),
                cells: vec![equal_cell("修改行"), changed_cell("1", "2")],
                note: None,
            },
            GridRow {
                kind: GridRowKind::SuspectedModified,
                old_row: Some(4),
                new_row: Some(4),
                cells: vec![equal_cell("疑似行"), changed_cell("1", "2")],
                note: None,
            },
            GridRow {
                kind: GridRowKind::Added,
                old_row: None,
                new_row: Some(5),
                cells: vec![
                    glyph_cell("新增行", GridCellKind::Added),
                    glyph_cell("3", GridCellKind::Added),
                ],
                note: None,
            },
            GridRow {
                kind: GridRowKind::Removed,
                old_row: Some(6),
                new_row: None,
                cells: vec![
                    glyph_cell("删除行", GridCellKind::Removed),
                    glyph_cell("4", GridCellKind::Removed),
                ],
                note: None,
            },
            GridRow {
                kind: GridRowKind::MovedTo,
                old_row: Some(7),
                new_row: Some(8),
                cells: vec![
                    glyph_cell("移入行", GridCellKind::Added),
                    glyph_cell("5", GridCellKind::Added),
                ],
                note: None,
            },
            GridRow {
                kind: GridRowKind::MovedFrom,
                old_row: Some(7),
                new_row: None,
                cells: vec![
                    glyph_cell("移出行", GridCellKind::Removed),
                    glyph_cell("5", GridCellKind::Removed),
                ],
                note: None,
            },
        ];
        table
    }

    #[test]
    fn table_row_cursor_moves_and_scrolls() {
        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output_with_table())));
        app.view = ViewMode::Table;
        let mut table = long_detail_table();
        table.rows = (1..=10)
            .map(|index| GridRow {
                kind: GridRowKind::Unchanged,
                old_row: Some(index),
                new_row: Some(index),
                cells: vec![equal_cell(&format!("第{index}项")), equal_cell("")],
                note: None,
            })
            .collect();
        app.table_view = Some(table);
        app.reset_table_navigation();
        app.table_page = 4;
        app.table_max_scroll = 100;

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.table_selected, 1);
        assert_eq!(app.table_scroll, 1, "选中行应保持在可视区内");

        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.table_selected, 9);
        assert_eq!(app.table_scroll, 9);

        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.table_selected, 0);
        assert_eq!(app.table_scroll, 3);

        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.table_selected, 4);
        assert_eq!(app.table_scroll, 4);
    }

    #[test]
    fn table_filter_cycles_with_tab_and_numbers() {
        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output_with_table())));
        app.view = ViewMode::Table;
        app.table_view = Some(filter_sample_table());
        app.reset_table_navigation();
        assert_eq!(app.table_filter, Filter::All);
        assert_eq!(app.table_row_count(), 7);
        assert_eq!(app.table_selected, 0);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.table_filter, Filter::Modified);
        assert_eq!(app.table_visible, vec![1]);
        assert_eq!(app.table_selected, 0);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.table_filter, Filter::Suspected);
        assert_eq!(app.table_visible, vec![2]);

        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.table_filter, Filter::Modified);

        app.handle_key(key(KeyCode::Char('6')));
        assert_eq!(app.table_filter, Filter::Moved);
        assert_eq!(app.table_visible, vec![5, 6]);

        app.handle_key(key(KeyCode::Char('4')));
        assert_eq!(app.table_filter, Filter::Added);
        assert_eq!(app.table_visible, vec![3]);

        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.table_filter, Filter::All);
        assert_eq!(app.table_row_count(), 7);
    }

    #[test]
    fn table_filter_labels_use_distinct_colors() {
        let colors: Vec<Option<Color>> = Filter::ORDER
            .iter()
            .map(|filter| filter.style().fg)
            .collect();
        for (index, color) in colors.iter().enumerate() {
            assert!(color.is_some(), "{colors:?}");
            assert!(
                !colors[..index].contains(color),
                "各筛选类型应使用不同颜色: {colors:?}"
            );
        }
    }

    #[test]
    fn table_view_filter_hides_other_rows_and_reports_empty() {
        let table = filter_sample_table();
        let visible = visible_table_rows(&table, Filter::Modified);
        assert_eq!(visible, vec![1]);
        let (lines, _) = table_view_lines(&table, 80, 0, &visible);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("修改行"), "{text}");
        for other in ["未变行", "疑似行", "新增行", "删除行", "移入行", "移出行"]
        {
            assert!(!text.contains(other), "筛选“修改”不应显示 {other}: {text}");
        }

        let sample = sample_grid();
        let empty = visible_table_rows(&sample, Filter::Suspected);
        assert!(empty.is_empty());
        let (lines, _) = table_view_lines(&sample, 80, 0, &empty);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("当前筛选无变化行"), "{text}");
        assert!(!text.contains("售电量"), "{text}");
    }

    #[test]
    fn brackets_switch_tables_while_tab_filters() {
        let mut output = revision_output_with_table();
        let mut second = sample_grid();
        second.table = 2;
        second.order = 2.0;
        output.tables.push(second);

        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(output)));
        app.view = ViewMode::Table;
        app.open_table_view();
        assert_eq!(app.table_view.as_ref().unwrap().table, 1);

        app.handle_key(key(KeyCode::Char(']')));
        assert_eq!(app.table_view.as_ref().unwrap().table, 2);
        app.handle_key(key(KeyCode::Char('[')));
        assert_eq!(app.table_view.as_ref().unwrap().table, 1);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.table_filter, Filter::Modified);
        assert_eq!(app.table_view.as_ref().unwrap().table, 1, "Tab 不应再切表");
        app.handle_key(key(KeyCode::Char(']')));
        assert_eq!(app.table_filter, Filter::Modified, "切表后应保留当前筛选");
    }

    #[test]
    fn table_detail_shows_revision_style_diff() {
        let table = long_detail_table();
        let lines = table_detail_lines(&table, 0);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("修改 · 第 30 行"), "{text}");
        assert!(text.contains("整改措施"), "{text}");
        assert!(text.contains("差异："), "{text}");
        assert!(
            text.contains("继续跟踪第30项，确保按期完成"),
            "未变片段应保留: {text}"
        );
        assert!(text.contains("并附现场照片"), "新增片段应完整显示: {text}");
        assert_eq!(
            text.matches("继续跟踪第30项").count(),
            1,
            "未变前缀不应在旧值/新值里重复: {text}"
        );
        assert!(
            !text.contains("旧：") && !text.contains("新："),
            "修改格应合并成修订模式，而不是整行旧值/新值: {text}"
        );
        assert!(!text.contains('…'), "行详情不应截断: {text}");

        let diff_line = lines
            .iter()
            .find(|line| line_text(line).contains("差异："))
            .unwrap();
        assert!(
            diff_line
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::CROSSED_OUT)),
            "删除片段应有删除线: {text}"
        );
        assert!(
            diff_line
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::UNDERLINED)),
            "新增片段应有下划线: {text}"
        );
    }

    #[test]
    fn table_detail_opens_closes_and_scrolls() {
        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output_with_table())));
        app.view = ViewMode::Table;
        app.table_view = Some(long_detail_table());
        app.reset_table_navigation();

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.table_detail, Some(0));
        assert_eq!(app.table_detail_scroll, 0);

        app.table_detail_page = 5;
        app.table_detail_max = 20;
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.table_detail_scroll, 1);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.table_detail_scroll, 20);

        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.table_detail, None, "q 应关闭行详情");
        assert!(!app.quit, "行详情里的 q 不应退出程序");
        assert_eq!(app.view, ViewMode::Table);

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.table_detail, Some(0));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.table_detail, None, "Enter 也应关闭行详情");

        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.table_detail, None);
        assert_eq!(app.view, ViewMode::Table, "关闭详情应留在表格视图");

        app.table_view = Some(TableGrid {
            rows: Vec::new(),
            ..long_detail_table()
        });
        app.reset_table_navigation();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.table_detail, None, "空表不应打开行详情");

        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.quit, "回到表格视图后 q 仍退出程序");
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

    fn equal_cell(text: &str) -> GridCell {
        GridCell {
            old: Some(text.to_string()),
            new: Some(text.to_string()),
            diff: None,
            kind: GridCellKind::Unchanged,
        }
    }

    fn changed_cell(old: &str, new: &str) -> GridCell {
        GridCell {
            old: Some(old.to_string()),
            new: Some(new.to_string()),
            diff: Some(crate::diff::para_diff(old, new)),
            kind: GridCellKind::Modified,
        }
    }

    fn glyph_cell(text: &str, kind: GridCellKind) -> GridCell {
        match kind {
            GridCellKind::Added => GridCell {
                old: None,
                new: Some(text.to_string()),
                diff: None,
                kind,
            },
            GridCellKind::Removed => GridCell {
                old: Some(text.to_string()),
                new: None,
                diff: None,
                kind,
            },
            _ => equal_cell(text),
        }
    }

    fn column(label: &str, kind: GridColumnKind) -> GridColumn {
        GridColumn {
            label: label.to_string(),
            old_label: (kind != GridColumnKind::Added).then(|| label.to_string()),
            old_index: (kind != GridColumnKind::Added).then_some(0),
            new_index: (kind != GridColumnKind::Removed).then_some(0),
            kind,
        }
    }

    fn all_rows(table: &TableGrid) -> Vec<usize> {
        (0..table.rows.len()).collect()
    }

    fn sample_grid() -> TableGrid {
        TableGrid {
            table: 1,
            caption: Some("指标完成情况".to_string()),
            order: 0.0,
            columns: vec![
                column("指标", GridColumnKind::Unchanged),
                column("完成率", GridColumnKind::Unchanged),
                column("备注", GridColumnKind::Added),
            ],
            rows: vec![
                GridRow {
                    kind: GridRowKind::Unchanged,
                    old_row: Some(2),
                    new_row: Some(2),
                    cells: vec![
                        equal_cell("售电量"),
                        equal_cell("100"),
                        glyph_cell("", GridCellKind::Unchanged),
                    ],
                    note: None,
                },
                GridRow {
                    kind: GridRowKind::Modified,
                    old_row: Some(3),
                    new_row: Some(3),
                    cells: vec![
                        equal_cell("完成率"),
                        changed_cell("80", "85"),
                        glyph_cell("新增说明", GridCellKind::Added),
                    ],
                    note: None,
                },
                GridRow {
                    kind: GridRowKind::Added,
                    old_row: None,
                    new_row: Some(4),
                    cells: vec![
                        glyph_cell("新指标", GridCellKind::Added),
                        glyph_cell("1", GridCellKind::Added),
                        glyph_cell("", GridCellKind::Added),
                    ],
                    note: None,
                },
            ],
            summary: crate::result::Summary {
                unchanged: 1,
                added: 1,
                modified: 1,
                ..Default::default()
            },
            whole: None,
        }
    }

    #[test]
    fn revision_tables_open_in_a_dedicated_view() {
        use ratatui::backend::TestBackend;

        let mut app = App::new(PathBuf::from("old.docx"), PathBuf::from("new.docx"), 80);
        app.apply(WorkerMsg::Done(Box::new(revision_output_with_table())));
        app.handle_key(key(KeyCode::Char('v')));
        assert_eq!(app.view, ViewMode::Revision);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.revision_table_count(), 1);
        assert_eq!(app.revision_table_lines.len(), 1);
        let compact = screen_text(terminal.backend().buffer());
        assert!(compact.contains("Enter查看表格修订"), "{compact}");
        assert!(
            !compact.contains("---+"),
            "修订视图不应渲染表格网格: {compact}"
        );

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view, ViewMode::Table);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let compact = screen_text(terminal.backend().buffer());
        assert!(compact.contains("表格修订"), "{compact}");
        assert!(compact.contains("---+"), "{compact}");

        app.handle_key(key(KeyCode::Char('v')));
        assert_eq!(app.view, ViewMode::Revision);
        assert!(app.table_view.is_none());
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view, ViewMode::Table);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.view, ViewMode::Revision);
    }

    fn screen_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut screen = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                screen.push_str(buffer[(x, y)].symbol());
            }
            screen.push('\n');
        }
        screen.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[test]
    fn table_view_renders_columns_rows_and_cell_marks() {
        let table = sample_grid();
        let (lines, _) = table_view_lines(&table, 80, 0, &all_rows(&table));
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            table_summary_text(&table).contains("未变 1 | 新增 1 | 修改 1"),
            "{}",
            table_summary_text(&table)
        );
        assert!(text.contains("|"), "{text}");
        assert!(text.contains("---"), "{text}");
        assert!(text.contains("+备注"), "{text}");
        assert!(text.contains("指标"), "{text}");
        assert!(text.contains("售电量"), "{text}");

        let modified = lines
            .iter()
            .find(|line| line_text(line).contains("完成率") && line_text(line).contains("80"))
            .unwrap();
        assert!(line_text(modified).contains("~"), "{text}");
        let deleted = modified
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "0")
            .unwrap();
        let inserted = modified
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "5")
            .unwrap();
        assert!(deleted.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert!(inserted.style.add_modifier.contains(Modifier::UNDERLINED));

        let added = lines
            .iter()
            .find(|line| line_text(line).contains("新指标"))
            .unwrap();
        assert!(line_text(added).contains("+"), "{text}");

        for line in &lines {
            assert!(
                UnicodeWidthStr::width(line_text(line).as_str()) <= 80,
                "{}",
                line_text(line)
            );
        }
        let grid_widths: Vec<usize> = lines
            .iter()
            .map(|line| line_text(line))
            .filter(|text| text.contains('|') || text.starts_with("------"))
            .map(|text| UnicodeWidthStr::width(text.as_str()))
            .collect();
        assert!(
            grid_widths.windows(2).all(|pair| pair[0] == pair[1]),
            "表头/分隔线/数据行应等宽: {grid_widths:?}\n{text}"
        );
    }

    #[test]
    fn table_placeholder_shows_entry_hint_only_when_focused() {
        let table = sample_grid();
        let idle = table_placeholder_line(&table, false, 120);
        let idle_text = line_text(&idle);
        assert!(idle_text.contains("表1（指标完成情况）"), "{idle_text}");
        assert!(!idle_text.contains("Enter"), "{idle_text}");

        let focused = table_placeholder_line(&table, true, 120);
        let focused_text = line_text(&focused);
        assert!(focused_text.starts_with("> "), "{focused_text}");
        assert!(
            focused_text.contains("Enter 查看表格修订"),
            "{focused_text}"
        );
        assert!(UnicodeWidthStr::width(focused_text.as_str()) <= 120);
        assert!(UnicodeWidthStr::width(focused_text.as_str()) >= 118);

        let narrow = table_placeholder_line(&table, true, 8);
        assert!(UnicodeWidthStr::width(line_text(&narrow).as_str()) <= 8);
        assert!(line_text(&narrow).ends_with('…'));
    }

    #[test]
    fn clip_line_keeps_prefix_and_ellipsis() {
        let line = Line::from(vec![
            Span::raw("▦ "),
            Span::raw("表1（指标完成情况）"),
            Span::raw("  未变 1"),
        ]);
        let clipped = clip_line(line, 16);
        let text = line_text(&clipped);
        assert_eq!(text, "▦ 表1（指标完成…", "{text}");
    }

    #[test]
    fn clip_spans_keeps_prefix_before_the_ellipsis() {
        let spans = text_spans(Some("继续跟踪第1项，确保按期完成"), DiffOp::Equal);
        let text: String = clip_spans(&spans, 24)
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(text.starts_with("继续跟踪第1项"), "{text}");
        assert!(text.ends_with('…'), "{text}");
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 24, "{text}");
    }

    #[test]
    fn table_view_handles_unchanged_tables_and_narrow_width() {
        let mut table = sample_grid();
        table.summary = crate::result::Summary {
            unchanged: 3,
            ..Default::default()
        };
        table.columns = vec![column("指标", GridColumnKind::Unchanged)];
        assert_eq!(table_summary_text(&table), "无变化");
        let (lines, _) = table_view_lines(&table, 80, 0, &all_rows(&table));
        assert!(lines.len() > 1, "表格修订视图仍显示完整网格");
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("标记："), "{text}");
        assert!(text.contains("售电量"), "{text}");

        let mut changed = sample_grid();
        changed.summary = crate::result::Summary {
            modified: 1,
            ..Default::default()
        };
        let (lines, max_hscroll) = table_view_lines(&changed, 16, 0, &all_rows(&changed));
        assert!(max_hscroll > 0, "窄终端应允许横向滚动");
        for line in &lines {
            assert!(UnicodeWidthStr::width(line_text(line).as_str()) <= 16);
        }
        let row_at =
            |hscroll| line_text(&table_view_lines(&changed, 16, hscroll, &all_rows(&changed)).0[3]);
        let first = row_at(0);
        let scrolled = row_at(12);
        assert_eq!(
            first.chars().take(6).collect::<String>(),
            scrolled.chars().take(6).collect::<String>(),
            "行号栏应固定"
        );
        assert_ne!(first, scrolled, "横向滚动应改变可见列");
    }

    #[test]
    fn table_view_keeps_min_column_width_and_scrolls_to_last_column() {
        let columns: Vec<GridColumn> = (0..10)
            .map(|index| column(&format!("指标{index}"), GridColumnKind::Unchanged))
            .collect();
        let cells: Vec<GridCell> = (0..10)
            .map(|index| equal_cell(&format!("2026年数据{index}")))
            .collect();
        let table = TableGrid {
            table: 1,
            caption: None,
            order: 0.0,
            columns,
            rows: vec![GridRow {
                kind: GridRowKind::Unchanged,
                old_row: Some(1),
                new_row: Some(1),
                cells,
                note: None,
            }],
            summary: crate::result::Summary {
                unchanged: 1,
                ..Default::default()
            },
            whole: None,
        };
        assert!(
            grid_widths(&table)
                .iter()
                .all(|width| *width >= GRID_MIN_COL),
            "列宽不应被压到最小宽度以下: {:?}",
            grid_widths(&table)
        );

        let (lines, max_hscroll) = table_view_lines(&table, 60, 0, &all_rows(&table));
        assert!(max_hscroll > 0, "超宽表格应可横向滚动");
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !text.contains('…'),
            "列有最小宽度后不应整列只剩省略号:\n{text}"
        );
        for line in &lines {
            assert!(UnicodeWidthStr::width(line_text(line).as_str()) <= 60);
        }

        let (scrolled, _) = table_view_lines(&table, 60, max_hscroll, &all_rows(&table));
        let scrolled_text: String = scrolled
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(scrolled_text.contains("指标9"), "{scrolled_text}");
        assert!(scrolled_text.contains("2026年数据9"), "{scrolled_text}");
        assert_eq!(
            line_text(&lines[2]).chars().take(6).collect::<String>(),
            line_text(&scrolled[2]).chars().take(6).collect::<String>(),
            "行号栏在横向滚动时保持不动"
        );
    }

    #[test]
    fn table_view_shows_renamed_header() {
        let table = TableGrid {
            table: 1,
            caption: None,
            order: 0.0,
            columns: vec![
                column("指标", GridColumnKind::Unchanged),
                GridColumn {
                    label: "本期".to_string(),
                    old_label: Some("本月".to_string()),
                    old_index: Some(1),
                    new_index: Some(1),
                    kind: GridColumnKind::Unchanged,
                },
            ],
            rows: vec![GridRow {
                kind: GridRowKind::Unchanged,
                old_row: Some(2),
                new_row: Some(2),
                cells: vec![equal_cell("销量"), equal_cell("1")],
                note: None,
            }],
            summary: crate::result::Summary {
                unchanged: 1,
                ..Default::default()
            },
            whole: None,
        };
        assert!(table_summary_text(&table).contains("改列名 1"));
        let (lines, _) = table_view_lines(&table, 80, 0, &all_rows(&table));
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("本月->本期"), "{text}");
        assert!(text.contains("销量"), "{text}");
        let widths: Vec<usize> = lines
            .iter()
            .map(|line| line_text(line))
            .filter(|text| text.contains('|'))
            .map(|text| UnicodeWidthStr::width(text.as_str()))
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "改列名后表格行宽应一致: {widths:?}\n{text}"
        );
    }

    #[test]
    fn table_view_marks_whole_table_and_moved_rows() {
        let mut whole = sample_grid();
        whole.columns = vec![
            column("甲", GridColumnKind::Removed),
            column("乙", GridColumnKind::Removed),
        ];
        whole.summary = crate::result::Summary {
            removed: 2,
            ..Default::default()
        };
        whole.whole = Some(Status::Removed);
        whole.rows = vec![
            GridRow {
                kind: GridRowKind::Removed,
                old_row: Some(2),
                new_row: None,
                cells: vec![
                    glyph_cell("甲", GridCellKind::Removed),
                    glyph_cell("1", GridCellKind::Removed),
                ],
                note: None,
            },
            GridRow {
                kind: GridRowKind::Removed,
                old_row: Some(3),
                new_row: None,
                cells: vec![
                    glyph_cell("乙", GridCellKind::Removed),
                    glyph_cell("2", GridCellKind::Removed),
                ],
                note: None,
            },
        ];
        assert_eq!(table_summary_text(&whole), "整表删除 | 共 2 行");
        let (lines, _) = table_view_lines(&whole, 80, 0, &all_rows(&whole));
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("-甲"), "{text}");
        assert!(text.contains("甲"), "{text}");
        assert!(text.contains("乙"), "{text}");

        let mut moved = sample_grid();
        moved.columns = vec![column("指标", GridColumnKind::Unchanged)];
        moved.summary = crate::result::Summary {
            moved: 1,
            unchanged: 1,
            ..Default::default()
        };
        moved.rows = vec![
            GridRow {
                kind: GridRowKind::Unchanged,
                old_row: Some(2),
                new_row: Some(2),
                cells: vec![equal_cell("甲")],
                note: None,
            },
            GridRow {
                kind: GridRowKind::MovedTo,
                old_row: Some(4),
                new_row: Some(3),
                cells: vec![glyph_cell("丙", GridCellKind::Added)],
                note: Some("移动自旧行 4".to_string()),
            },
            GridRow {
                kind: GridRowKind::MovedFrom,
                old_row: Some(4),
                new_row: None,
                cells: vec![glyph_cell("丙", GridCellKind::Removed)],
                note: Some("移动至新行 3".to_string()),
            },
        ];
        let (lines, _) = table_view_lines(&moved, 80, 0, &all_rows(&moved));
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("> 移入  < 移出"), "{text}");
        assert!(text.contains("（移动：旧4->新3）"), "{text}");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line_text(line).starts_with('>'))
                .count(),
            1
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line_text(line).starts_with('<'))
                .count(),
            1
        );
        let widths: Vec<usize> = lines
            .iter()
            .map(|line| line_text(line))
            .filter(|text| text.contains('|'))
            .map(|text| UnicodeWidthStr::width(text.as_str()))
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "表格行宽应一致: {widths:?}\n{text}"
        );
    }

    #[test]
    fn table_view_moved_rows_align_with_unchanged_rows() {
        let table = TableGrid {
            table: 2,
            caption: None,
            order: 0.0,
            columns: vec![
                column("姓名", GridColumnKind::Unchanged),
                column("部门", GridColumnKind::Unchanged),
            ],
            rows: vec![
                GridRow {
                    kind: GridRowKind::MovedTo,
                    old_row: Some(4),
                    new_row: Some(2),
                    cells: vec![
                        glyph_cell("丙", GridCellKind::Added),
                        glyph_cell("营销部", GridCellKind::Added),
                    ],
                    note: None,
                },
                GridRow {
                    kind: GridRowKind::Unchanged,
                    old_row: Some(2),
                    new_row: Some(3),
                    cells: vec![equal_cell("甲"), equal_cell("办公室")],
                    note: None,
                },
                GridRow {
                    kind: GridRowKind::MovedFrom,
                    old_row: Some(4),
                    new_row: None,
                    cells: vec![
                        glyph_cell("丙", GridCellKind::Removed),
                        glyph_cell("营销部", GridCellKind::Removed),
                    ],
                    note: None,
                },
            ],
            summary: crate::result::Summary {
                unchanged: 1,
                moved: 1,
                ..Default::default()
            },
            whole: None,
        };
        let (lines, _) = table_view_lines(&table, 120, 0, &all_rows(&table));
        let widths: Vec<usize> = lines
            .iter()
            .map(|line| line_text(line))
            .filter(|text| text.contains('|'))
            .map(|text| UnicodeWidthStr::width(text.as_str()))
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "移动行与普通行应等宽: {widths:?}\n{}",
            lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
        );
        let dividers: Vec<usize> = lines
            .iter()
            .map(|line| line_text(line))
            .filter(|text| text.contains('|'))
            .map(|text| {
                let index = text.find('|').unwrap();
                UnicodeWidthStr::width(&text[..index])
            })
            .collect();
        assert!(
            dividers.windows(2).all(|pair| pair[0] == pair[1]),
            "列分隔线位置应一致: {dividers:?}\n{}",
            lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn table_view_keeps_rows_aligned_for_multiline_cells() {
        let table = TableGrid {
            table: 1,
            caption: None,
            order: 0.0,
            columns: vec![
                column("事项", GridColumnKind::Unchanged),
                column("说明", GridColumnKind::Unchanged),
                column("结果", GridColumnKind::Unchanged),
            ],
            rows: vec![
                GridRow {
                    kind: GridRowKind::Modified,
                    old_row: Some(2),
                    new_row: Some(2),
                    cells: vec![
                        equal_cell("甲"),
                        changed_cell("第一行\n第二行", "第一行\n第二行改"),
                        equal_cell("正常"),
                    ],
                    note: None,
                },
                GridRow {
                    kind: GridRowKind::Unchanged,
                    old_row: Some(3),
                    new_row: Some(3),
                    cells: vec![equal_cell("乙"), equal_cell("单行"), equal_cell("正常")],
                    note: None,
                },
            ],
            summary: crate::result::Summary {
                unchanged: 1,
                modified: 1,
                ..Default::default()
            },
            whole: None,
        };
        let (lines, _) = table_view_lines(&table, 80, 0, &all_rows(&table));
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            lines.iter().all(|line| !line_text(line).contains('\n')),
            "单元格换行应转成空格: {text}"
        );
        assert!(text.contains("第一行 第二行改"), "{text}");
        let bar_positions: Vec<Vec<usize>> = lines
            .iter()
            .map(|line| line_text(line))
            .filter(|text| text.contains('|'))
            .map(|text| {
                text.match_indices('|')
                    .map(|(index, _)| UnicodeWidthStr::width(&text[..index]))
                    .collect()
            })
            .collect();
        assert!(
            bar_positions.windows(2).all(|pair| pair[0] == pair[1]),
            "多段落单元格不应撑开竖线: {bar_positions:?}\n{text}"
        );
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
            tables: Vec::new(),
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
