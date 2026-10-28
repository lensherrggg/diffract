use std::collections::BTreeSet;
use std::io;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::Config;
use crate::matching::Progress;
use crate::result::Status;
use crate::xlsx::model::CellValue;
use crate::xlsx::result::{GridCell, GridRow, SheetDiff, XlsxOutput};

use super::Filter;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

enum WorkerMsg {
    Progress(Progress),
    Done(Box<XlsxOutput>),
    Failed(String),
}

enum State {
    Running {
        stage: String,
        step: Option<(String, usize, usize)>,
    },
    Ready(Box<XlsxOutput>),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Grid,
    List,
}

/// 最上层的浮层；开、关或互相切换时强制重绘一帧空白格，清掉宽字符尾列残留的底色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    RowDetail,
}

struct App {
    old: PathBuf,
    new: PathBuf,
    state: State,
    view: ViewMode,
    sheet_index: usize,
    grid_row: usize,
    grid_col: usize,
    row_offset: usize,
    col_offset: usize,
    list_selected: usize,
    list_state: TableState,
    filter: Filter,
    changes: Vec<(usize, usize)>,
    changed_rows: BTreeSet<usize>,
    changed_cols: BTreeSet<usize>,
    reset_change_anchor: bool,
    last_visible_rows: usize,
    last_visible_cols: usize,
    last_visible_list_rows: usize,
    center_pending: bool,
    flash: Option<(usize, usize, usize, Instant)>,
    notice: Option<(String, Instant)>,
    help: bool,
    /// 整行对比浮层打开时记录的行下标。
    row_detail: Option<usize>,
    row_detail_scroll: u16,
    row_detail_max: u16,
    row_detail_page: u16,
    last_overlay: Overlay,
    quit: bool,
    fatal: bool,
    started: Instant,
    tick: usize,
}

impl App {
    fn new(old: PathBuf, new: PathBuf) -> Self {
        let mut list_state = TableState::default();
        list_state.select(Some(0));
        Self {
            old,
            new,
            state: State::Running {
                stage: "准备比较".to_string(),
                step: None,
            },
            view: ViewMode::Grid,
            sheet_index: 0,
            grid_row: 0,
            grid_col: 0,
            row_offset: 0,
            col_offset: 0,
            list_selected: 0,
            list_state,
            filter: Filter::All,
            changes: Vec::new(),
            changed_rows: BTreeSet::new(),
            changed_cols: BTreeSet::new(),
            reset_change_anchor: true,
            last_visible_rows: 10,
            last_visible_cols: 8,
            last_visible_list_rows: 10,
            center_pending: false,
            flash: None,
            notice: None,
            help: false,
            row_detail: None,
            row_detail_scroll: 0,
            row_detail_max: 0,
            row_detail_page: 1,
            last_overlay: Overlay::None,
            quit: false,
            fatal: false,
            started: Instant::now(),
            tick: 0,
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
                self.state = State::Ready(output);
                self.sheet_index = 0;
                self.grid_row = 0;
                self.grid_col = 0;
                self.row_offset = 0;
                self.col_offset = 0;
                self.list_selected = 0;
                self.list_state.select(Some(0));
                self.normalize_grid_row();
                self.rebuild_changes();
                self.reset_change_anchor = true;
                self.close_row_detail();
            }
            WorkerMsg::Failed(error) => {
                log::error!("Excel 比较失败：{error}");
                self.state = State::Failed(error);
                self.fatal = true;
            }
        }
    }

    fn output(&self) -> Option<&XlsxOutput> {
        match &self.state {
            State::Ready(output) => Some(output),
            _ => None,
        }
    }

    fn sheet(&self) -> Option<&SheetDiff> {
        let output = self.output()?;
        output.sheets.get(self.sheet_index)
    }

    fn sheet_count(&self) -> usize {
        self.output().map(|output| output.sheets.len()).unwrap_or(0)
    }

    fn data_row_indices(&self) -> Vec<usize> {
        let Some(sheet) = self.sheet() else {
            return Vec::new();
        };
        let header_rows = usize::from(sheet.header_row.is_some());
        sheet
            .rows
            .iter()
            .enumerate()
            .skip(header_rows)
            .map(|(index, _)| index)
            .collect()
    }

    fn normalize_grid_row(&mut self) {
        let indices = self.data_row_indices();
        if indices.is_empty() {
            self.grid_row = 0;
        } else if !indices.contains(&self.grid_row) {
            self.grid_row = indices[0];
        }
    }

    fn move_grid(&mut self, delta_row: isize, delta_col: isize) {
        let Some(sheet) = self.sheet() else {
            return;
        };
        let row_indices = self.data_row_indices();
        let col_count = sheet.columns.len();
        if row_indices.is_empty() || col_count == 0 {
            return;
        }
        let current = row_indices
            .iter()
            .position(|index| *index == self.grid_row)
            .unwrap_or(0);
        let next = (current as isize + delta_row).clamp(0, row_indices.len() as isize - 1) as usize;
        self.grid_row = row_indices[next];
        self.grid_col =
            (self.grid_col as isize + delta_col).clamp(0, col_count as isize - 1) as usize;
        self.reset_change_anchor = false;
        self.notice = None;
    }

    fn next_sheet(&mut self) {
        let count = self.sheet_count();
        if count == 0 {
            return;
        }
        self.sheet_index = (self.sheet_index + 1) % count;
        self.grid_row = 0;
        self.grid_col = 0;
        self.row_offset = 0;
        self.col_offset = 0;
        self.list_selected = 0;
        self.list_state.select(Some(0));
        self.normalize_grid_row();
        self.rebuild_changes();
        self.reset_change_anchor = true;
        self.close_row_detail();
    }

    fn filtered_cells(&self) -> Vec<(usize, usize)> {
        let Some(sheet) = self.sheet() else {
            return Vec::new();
        };
        let mut cells = Vec::new();
        for (row_index, row) in sheet.rows.iter().enumerate() {
            for (cell_index, cell) in row.cells.iter().enumerate() {
                if is_visible_change(cell) && self.filter.matches(cell.status) {
                    cells.push((row_index, cell_index));
                }
            }
        }
        cells
    }

    fn rebuild_changes(&mut self) {
        let mut changes = Vec::new();
        let mut changed_rows = BTreeSet::new();
        let mut changed_cols = BTreeSet::new();
        if let Some(sheet) = self.sheet() {
            let header_offset = usize::from(sheet.header_row.is_some());
            for (row_index, row) in sheet.rows.iter().enumerate().skip(header_offset) {
                for (col_index, cell) in row.cells.iter().enumerate() {
                    if is_visible_change(cell) && self.filter.matches(cell.status) {
                        changes.push((row_index, col_index));
                        changed_rows.insert(row_index);
                        changed_cols.insert(col_index);
                    }
                }
            }
        }
        self.changes = changes;
        self.changed_rows = changed_rows;
        self.changed_cols = changed_cols;
    }

    fn current_cell(&self) -> Option<&GridCell> {
        let sheet = self.sheet()?;
        sheet
            .rows
            .get(self.grid_row)
            .and_then(|row| row.cells.get(self.grid_col))
    }

    fn current_change_index(&self) -> Option<usize> {
        self.changes
            .binary_search(&(self.grid_row, self.grid_col))
            .ok()
    }

    fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    fn move_to(&mut self, row: usize, column: usize) {
        self.grid_row = row;
        self.grid_col = column;
        self.flash = Some((self.sheet_index, row, column, Instant::now()));
        self.center_pending = true;
        self.reset_change_anchor = false;
        self.notice = None;
    }

    fn jump_to_change(&mut self, index: usize) {
        let Some(&(row, column)) = self.changes.get(index) else {
            return;
        };
        self.move_to(row, column);
    }

    fn next_change(&mut self, forward: bool) {
        if self.changes.is_empty() {
            self.set_notice("当前筛选无变化");
            return;
        }
        let length = self.changes.len();
        let current = (self.grid_row, self.grid_col);
        let (index, wrapped) = if self.reset_change_anchor {
            self.reset_change_anchor = false;
            (if forward { 0 } else { length - 1 }, false)
        } else if forward {
            let next = self
                .changes
                .partition_point(|position| *position <= current);
            if next < length {
                (next, false)
            } else {
                (0, true)
            }
        } else {
            let previous = self.changes.partition_point(|position| *position < current);
            if previous > 0 {
                (previous - 1, false)
            } else {
                (length - 1, true)
            }
        };
        self.jump_to_change(index);
        if wrapped {
            self.set_notice(if forward {
                "已回到第一个变化"
            } else {
                "已到最后一个变化"
            });
        }
    }

    fn closest_change_in_row(&self, row: usize, preferred_column: usize) -> Option<(usize, usize)> {
        self.changes
            .iter()
            .filter(|(changed_row, _)| *changed_row == row)
            .min_by_key(|(_, column)| column.abs_diff(preferred_column))
            .copied()
    }

    fn closest_change_in_column(
        &self,
        column: usize,
        preferred_row: usize,
    ) -> Option<(usize, usize)> {
        self.changes
            .iter()
            .filter(|(_, changed_column)| *changed_column == column)
            .min_by_key(|(row, _)| row.abs_diff(preferred_row))
            .copied()
    }

    fn change_count_in_row(&self, row: usize) -> usize {
        self.changes
            .iter()
            .filter(|(changed_row, _)| *changed_row == row)
            .count()
    }

    fn change_count_in_column(&self, column: usize) -> usize {
        self.changes
            .iter()
            .filter(|(_, changed_column)| *changed_column == column)
            .count()
    }

    fn row_number_at(&self, row: usize) -> usize {
        self.sheet()
            .and_then(|sheet| sheet.rows.get(row))
            .map(|row| row.number())
            .unwrap_or(row + 1)
    }

    fn column_label_at(&self, column: usize) -> String {
        self.sheet()
            .and_then(|sheet| sheet.columns.get(column))
            .map(|column| column.label.clone())
            .unwrap_or_else(|| format!("第 {} 列", column + 1))
    }

    fn next_changed_row(&mut self, forward: bool) {
        if self.changed_rows.is_empty() {
            self.set_notice("当前筛选无变化");
            return;
        }
        let (row, wrapped) = if forward {
            match self
                .changed_rows
                .range((Excluded(self.grid_row), Unbounded))
                .next()
            {
                Some(row) => (*row, false),
                None => (*self.changed_rows.first().unwrap(), true),
            }
        } else {
            match self.changed_rows.range(..self.grid_row).next_back() {
                Some(row) => (*row, false),
                None => (*self.changed_rows.last().unwrap(), true),
            }
        };
        let column_count = self.sheet().map(|sheet| sheet.columns.len()).unwrap_or(0);
        let column = if self.grid_col < column_count {
            self.grid_col
        } else {
            self.closest_change_in_row(row, self.grid_col)
                .map(|(_, column)| column)
                .unwrap_or_else(|| column_count.saturating_sub(1))
        };
        self.move_to(row, column);
        let count = self.change_count_in_row(row);
        let prefix = if wrapped { "已循环 · " } else { "" };
        self.set_notice(format!(
            "{prefix}行 {} 有 {count} 处变化",
            self.row_number_at(row)
        ));
    }

    fn next_changed_column(&mut self, forward: bool) {
        if self.changed_cols.is_empty() {
            self.set_notice("当前筛选无变化");
            return;
        }
        let (column, wrapped) = if forward {
            match self
                .changed_cols
                .range((Excluded(self.grid_col), Unbounded))
                .next()
            {
                Some(column) => (*column, false),
                None => (*self.changed_cols.first().unwrap(), true),
            }
        } else {
            match self.changed_cols.range(..self.grid_col).next_back() {
                Some(column) => (*column, false),
                None => (*self.changed_cols.last().unwrap(), true),
            }
        };
        let row_is_valid = self.sheet().is_some_and(|sheet| {
            let header_offset = usize::from(sheet.header_row.is_some());
            self.grid_row >= header_offset && sheet.rows.get(self.grid_row).is_some()
        });
        let row = if row_is_valid {
            self.grid_row
        } else {
            self.closest_change_in_column(column, self.grid_row)
                .map(|(row, _)| row)
                .unwrap_or(self.grid_row)
        };
        self.move_to(row, column);
        let count = self.change_count_in_column(column);
        let prefix = if wrapped { "已循环 · " } else { "" };
        self.set_notice(format!(
            "{prefix}列 {} 有 {count} 处变化",
            self.column_label_at(column)
        ));
    }

    fn jump_to_changed_sheet(&mut self) {
        let Some(output) = self.output() else {
            return;
        };
        let candidates: Vec<usize> = output
            .sheets
            .iter()
            .enumerate()
            .filter(|(_, sheet)| {
                sheet
                    .rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .any(|cell| is_visible_change(cell) && self.filter.matches(cell.status))
            })
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            self.set_notice("当前筛选无变化");
            return;
        }
        let target = candidates
            .iter()
            .find(|index| **index > self.sheet_index)
            .copied()
            .unwrap_or(candidates[0]);
        if target == self.sheet_index {
            self.set_notice("没有其他有变化的 sheet");
            return;
        }
        self.sheet_index = target;
        self.grid_row = 0;
        self.grid_col = 0;
        self.row_offset = 0;
        self.col_offset = 0;
        self.list_selected = 0;
        self.list_state.select(Some(0));
        self.rebuild_changes();
        self.reset_change_anchor = true;
        self.normalize_grid_row();
        self.close_row_detail();
        if self.changes.is_empty() {
            self.set_notice("当前筛选无变化");
            return;
        }
        self.jump_to_change(0);
        let name = self
            .sheet()
            .map(|sheet| sheet.name.clone())
            .unwrap_or_default();
        let count = self.changes.len();
        self.set_notice(format!("工作表 {name} · 可跳转 {count}"));
    }

    fn jump_to_first_or_last_row(&mut self, last: bool) {
        let indices = self.data_row_indices();
        if let Some(row) = if last {
            indices.last()
        } else {
            indices.first()
        } {
            self.move_to(*row, self.grid_col);
        }
    }

    fn page_step(&self) -> isize {
        match self.view {
            ViewMode::Grid => self.last_visible_rows.max(2) as isize,
            ViewMode::List => self.last_visible_list_rows.max(2) as isize,
        }
    }

    fn half_page_step(&self) -> isize {
        (self.page_step() / 2).max(1)
    }

    fn scroll_rows(&mut self, delta: isize) {
        match self.view {
            ViewMode::Grid => self.move_grid(delta, 0),
            ViewMode::List => self.move_list(delta),
        }
    }

    fn toggle_view(&mut self) {
        self.notice = None;
        match self.view {
            ViewMode::Grid => {
                let cells = self.filtered_cells();
                if !cells.is_empty() {
                    let index = cells
                        .binary_search(&(self.grid_row, self.grid_col))
                        .unwrap_or_else(|insert| insert.min(cells.len() - 1));
                    self.list_selected = index;
                    self.list_state.select(Some(index));
                }
                self.view = ViewMode::List;
            }
            ViewMode::List => {
                if let Some(&(row, column)) = self.filtered_cells().get(self.list_selected) {
                    if !(self.sheet().is_some_and(|sheet| sheet.header_row.is_some()) && row == 0) {
                        self.grid_row = row;
                        self.grid_col = column;
                        self.center_pending = true;
                        self.reset_change_anchor = false;
                    }
                }
                self.view = ViewMode::Grid;
            }
        }
    }

    fn filter_count(&self, filter: Filter) -> usize {
        let Some(sheet) = self.sheet() else {
            return 0;
        };
        sheet
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .filter(|cell| is_visible_change(cell) && filter.matches(cell.status))
            .count()
    }

    fn set_filter(&mut self, filter: Filter) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.rebuild_changes();
        self.reset_change_anchor = true;
        self.list_selected = 0;
        self.list_state.select(Some(0));
        self.notice = None;
    }

    /// 当前最上层的浮层。`draw` 用它判断浮层是否刚开合过。
    fn overlay(&self) -> Overlay {
        if self.help {
            Overlay::Help
        } else if self.row_detail.is_some() {
            Overlay::RowDetail
        } else {
            Overlay::None
        }
    }

    fn open_row_detail(&mut self) {
        if !self.data_row_indices().contains(&self.grid_row) {
            return;
        }
        self.row_detail = Some(self.grid_row);
        self.row_detail_scroll = 0;
        self.row_detail_max = 0;
        self.row_detail_page = 1;
    }

    fn close_row_detail(&mut self) {
        self.row_detail = None;
        self.row_detail_scroll = 0;
        self.row_detail_max = 0;
    }

    fn scroll_row_detail(&mut self, delta: isize) {
        let current = self.row_detail_scroll as isize;
        let max = self.row_detail_max as isize;
        self.row_detail_scroll = current.saturating_add(delta).clamp(0, max) as u16;
    }

    fn handle_row_detail_key(&mut self, key: KeyEvent) {
        let page = self.row_detail_page.max(1) as isize;
        let half = (page / 2).max(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.close_row_detail(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_row_detail(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_row_detail(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_row_detail(-page),
            KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char(' ') => {
                self.scroll_row_detail(page);
            }
            KeyCode::Char('u') => self.scroll_row_detail(-half),
            KeyCode::Char('d') => self.scroll_row_detail(half),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_row_detail(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.scroll_row_detail(isize::MAX / 2),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
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
        if matches!(self.state, State::Running { .. } | State::Failed(_)) {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                self.quit = true;
            }
            return;
        }

        if self.row_detail.is_some() {
            self.handle_row_detail_key(key);
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let page = self.page_step();
            let half = self.half_page_step();
            match key.code {
                KeyCode::Char('f') => {
                    self.scroll_rows(page);
                    return;
                }
                KeyCode::Char('b') => {
                    self.scroll_rows(-page);
                    return;
                }
                KeyCode::Char('d') => {
                    self.scroll_rows(half);
                    return;
                }
                KeyCode::Char('u') => {
                    self.scroll_rows(-half);
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('s') => self.next_sheet(),
            KeyCode::Char('S') => self.jump_to_changed_sheet(),
            KeyCode::Char('v') => self.toggle_view(),
            KeyCode::Char('f') => self.scroll_rows(self.page_step()),
            KeyCode::Char('b') => self.scroll_rows(-self.page_step()),
            KeyCode::Char('d') => self.scroll_rows(self.half_page_step()),
            KeyCode::Char('u') => self.scroll_rows(-self.half_page_step()),
            KeyCode::Tab => self.set_filter(self.filter.cycle(1)),
            KeyCode::BackTab => self.set_filter(self.filter.cycle(-1)),
            KeyCode::Char(ch @ '1'..='6') => {
                let index = ch as usize - '1' as usize;
                if let Some(filter) = Filter::ORDER.get(index) {
                    self.set_filter(*filter);
                }
            }
            code if self.view == ViewMode::Grid => match code {
                KeyCode::Up | KeyCode::Char('k') => self.move_grid(-1, 0),
                KeyCode::Down | KeyCode::Char('j') => self.move_grid(1, 0),
                KeyCode::Left | KeyCode::Char('h') => self.move_grid(0, -1),
                KeyCode::Right | KeyCode::Char('l') => self.move_grid(0, 1),
                KeyCode::PageUp => self.scroll_rows(-self.page_step()),
                KeyCode::PageDown => self.scroll_rows(self.page_step()),
                KeyCode::Home | KeyCode::Char('g') => self.jump_to_first_or_last_row(false),
                KeyCode::End | KeyCode::Char('G') => self.jump_to_first_or_last_row(true),
                KeyCode::Char('n') => self.next_change(true),
                KeyCode::Char('N') => self.next_change(false),
                KeyCode::Char(']') => self.next_changed_row(true),
                KeyCode::Char('[') => self.next_changed_row(false),
                KeyCode::Char('}') => self.next_changed_column(true),
                KeyCode::Char('{') => self.next_changed_column(false),
                KeyCode::Enter => self.open_row_detail(),
                _ => {}
            },
            code => match code {
                KeyCode::Up | KeyCode::Char('k') => self.move_list(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_list(1),
                KeyCode::Char('n') => self.move_list(1),
                KeyCode::Char('N') => self.move_list(-1),
                KeyCode::Char(']') => self.move_list_changed_row(true),
                KeyCode::Char('[') => self.move_list_changed_row(false),
                KeyCode::Char('}') => self.move_list_changed_column(true),
                KeyCode::Char('{') => self.move_list_changed_column(false),
                KeyCode::PageUp => self.scroll_rows(-self.page_step()),
                KeyCode::PageDown => self.scroll_rows(self.page_step()),
                KeyCode::Home | KeyCode::Char('g') => self.move_list(isize::MIN / 2),
                KeyCode::End | KeyCode::Char('G') => self.move_list(isize::MAX / 2),
                KeyCode::Enter => self.jump_to_grid(),
                _ => {}
            },
        }
    }

    fn move_list(&mut self, delta: isize) {
        let count = self.filtered_cells().len();
        if count == 0 {
            return;
        }
        self.list_selected =
            (self.list_selected as isize + delta).clamp(0, count as isize - 1) as usize;
        self.list_state.select(Some(self.list_selected));
        self.reset_change_anchor = false;
        self.notice = None;
    }

    fn move_list_changed_row(&mut self, forward: bool) {
        let cells = self.filtered_cells();
        if cells.is_empty() {
            self.set_notice("当前筛选无变化");
            return;
        }
        let current_row = cells
            .get(self.list_selected)
            .map(|(row, _)| *row)
            .unwrap_or(0);
        for step in 1..cells.len() {
            let index = if forward {
                (self.list_selected + step) % cells.len()
            } else {
                (self.list_selected + cells.len() - step) % cells.len()
            };
            if cells[index].0 != current_row {
                self.list_selected = index;
                self.list_state.select(Some(index));
                return;
            }
        }
        self.set_notice("当前行没有其他变化");
    }

    fn move_list_changed_column(&mut self, forward: bool) {
        let cells = self.filtered_cells();
        if cells.is_empty() {
            self.set_notice("当前筛选无变化");
            return;
        }
        let current_column = cells
            .get(self.list_selected)
            .map(|(_, column)| *column)
            .unwrap_or(0);
        for step in 1..cells.len() {
            let index = if forward {
                (self.list_selected + step) % cells.len()
            } else {
                (self.list_selected + cells.len() - step) % cells.len()
            };
            if cells[index].1 != current_column {
                self.list_selected = index;
                self.list_state.select(Some(index));
                return;
            }
        }
        self.set_notice("当前列没有其他变化");
    }

    fn jump_to_grid(&mut self) {
        let Some(sheet) = self.sheet() else {
            return;
        };
        let cells = self.filtered_cells();
        let Some(&(row, column)) = cells.get(self.list_selected) else {
            return;
        };
        if sheet.header_row.is_some() && row == 0 {
            return;
        }
        self.move_to(row, column);
        self.view = ViewMode::Grid;
    }
}

pub fn run(
    terminal: &mut TuiTerminal,
    old: PathBuf,
    new: PathBuf,
    config: Config,
) -> Result<ExitCode> {
    let ui_old = old.clone();
    let ui_new = new.clone();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut progress = |update| {
            let _ = sender.send(WorkerMsg::Progress(update));
        };
        match crate::xlsx::compare_files(&old, &new, &config, &mut progress) {
            Ok(output) => {
                let _ = sender.send(WorkerMsg::Done(Box::new(output)));
            }
            Err(error) => {
                let _ = sender.send(WorkerMsg::Failed(format!("{error:#}")));
            }
        }
    });

    let mut app = App::new(ui_old, ui_new);
    event_loop(terminal, &mut app, receiver)?;
    Ok(if app.fatal {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

fn event_loop(
    terminal: &mut TuiTerminal,
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
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }
        app.tick = app.tick.wrapping_add(1);
    }
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(8),
        Constraint::Length(8),
        Constraint::Length(2),
    ])
    .split(area);

    draw_header(frame, chunks[0], app);
    match &app.state {
        State::Running { .. } => draw_running(frame, chunks[1], app),
        State::Failed(error) => {
            frame.render_widget(
                Paragraph::new(format!("Excel 比较失败\n\n{error}"))
                    .style(Style::default().fg(Color::Red))
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title("错误")),
                chunks[1],
            );
        }
        State::Ready(_) => match app.view {
            ViewMode::Grid => draw_grid(frame, chunks[1], app),
            ViewMode::List => draw_list(frame, chunks[1], app),
        },
    }
    draw_detail(frame, chunks[2], app);
    draw_footer(frame, chunks[3], app);
    if app.row_detail.is_some() {
        draw_row_detail(frame, chunks[1].union(chunks[2]), app);
    }
    if app.help {
        draw_help(frame, area);
    }

    let overlay = app.overlay();
    if overlay != app.last_overlay {
        super::force_repaint_blank_cells(frame);
        app.last_overlay = overlay;
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("diffRact  ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{} ↔ {}", file_name(&app.old), file_name(&app.new))),
    ]));
    if let Some(output) = app.output() {
        let sheet_tabs = output
            .sheets
            .iter()
            .enumerate()
            .map(|(index, sheet)| {
                let count = sheet
                    .rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .filter(|cell| is_visible_change(cell) && app.filter.matches(cell.status))
                    .count();
                let text = if count > 0 {
                    let label = if app.filter == Filter::All {
                        status_text(sheet.status)
                    } else {
                        app.filter.label()
                    };
                    format!("{}（{} {}）", sheet.name, label, count)
                } else {
                    format!("{}（未变）", sheet.name)
                };
                if index == app.sheet_index {
                    format!("[{text}]")
                } else {
                    text
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(Line::from(vec![
            Span::styled("工作表  ", Style::default().fg(Color::DarkGray)),
            Span::raw(sheet_tabs),
        ]));
        lines.push(Line::from(Span::styled(
            summary_line(output),
            Style::default().fg(Color::Gray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Excel 引擎 · 比较值与公式文本（不求值重算）",
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::from(Span::styled(
            app_note(app),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn app_note(app: &App) -> &'static str {
    let _ = app;
    "首版：多工作表、行列对齐、单元格差异；样式和图表不参与比较"
}

fn summary_line(output: &XlsxOutput) -> String {
    let summary = &output.summary;
    format!(
        "单元格 修改 {} 新增 {} 删除 {} 移动 {} · 行 新增 {} 删除 {} 移动 {} · 列 新增 {} 删除 {} 移动 {} · 用时 {:.2}s",
        summary.cells_modified,
        summary.cells_added,
        summary.cells_removed,
        summary.cells_moved,
        summary.rows_added,
        summary.rows_removed,
        summary.rows_moved,
        summary.columns_added,
        summary.columns_removed,
        summary.columns_moved,
        output.elapsed.as_secs_f64()
    )
}

fn draw_running(frame: &mut Frame, area: Rect, app: &App) {
    let (stage, step) = match &app.state {
        State::Running { stage, step } => (stage, step),
        _ => return,
    };
    let block = Block::default().borders(Borders::ALL).title("比较中");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![Line::from(Span::styled(
        stage.clone(),
        Style::default().fg(Color::Cyan),
    ))];
    if let Some((label, done, total)) = step {
        lines.push(Line::from(format!("{label} {done}/{total}")));
    }
    lines.push(Line::from(Span::styled(
        format!("已用时 {:.1}s", app.started.elapsed().as_secs_f64()),
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_grid(frame: &mut Frame, area: Rect, app: &mut App) {
    app.normalize_grid_row();
    let Some(sheet) = app.sheet() else {
        frame.render_widget(
            Paragraph::new("没有工作表").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    };
    let sheet_name = sheet.name.clone();
    let column_count = sheet.columns.len();
    let total_rows = sheet.rows.len();
    let header_row = sheet.header_row;
    let header_hint = header_row_label(header_row);
    let inner = Block::default().borders(Borders::ALL).inner(area);

    if total_rows == 0 || column_count == 0 {
        frame.render_widget(
            Paragraph::new("工作表没有非空单元格")
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(format!(" {sheet_name} · 网格 · {header_hint} ")),
                ),
            area,
        );
        return;
    }

    let data_rows = app.data_row_indices();
    let row_count = data_rows.len();
    if row_count == 0 {
        frame.render_widget(
            Paragraph::new("只有标题/表头行，没有数据行")
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(format!(" {sheet_name} · 网格 · {header_hint} ")),
                ),
            area,
        );
        return;
    }

    let data_width = 12usize;
    let row_number_width = 6usize;
    let available = inner.width.saturating_sub(row_number_width as u16 + 1) as usize;
    let visible_columns = (available / (data_width + 1)).max(1);
    let visible_rows = inner.height.saturating_sub(1).max(1) as usize;
    app.last_visible_rows = visible_rows;
    app.last_visible_cols = visible_columns;

    let cursor_position = data_rows
        .iter()
        .position(|index| *index == app.grid_row)
        .unwrap_or(0);

    if app.center_pending {
        app.col_offset = app
            .grid_col
            .saturating_sub(visible_columns / 2)
            .min(column_count.saturating_sub(visible_columns));
        app.row_offset = cursor_position
            .saturating_sub(visible_rows / 2)
            .min(row_count.saturating_sub(visible_rows));
        app.center_pending = false;
    } else {
        if app.grid_col < app.col_offset {
            app.col_offset = app.grid_col;
        } else if app.grid_col >= app.col_offset + visible_columns {
            app.col_offset = app.grid_col + 1 - visible_columns;
        }
        app.col_offset = app
            .col_offset
            .min(column_count.saturating_sub(visible_columns));

        if cursor_position < app.row_offset {
            app.row_offset = cursor_position;
        } else if cursor_position >= app.row_offset + visible_rows {
            app.row_offset = cursor_position + 1 - visible_rows;
        }
        app.row_offset = app.row_offset.min(row_count.saturating_sub(visible_rows));
    }

    let end_column = (app.col_offset + visible_columns).min(column_count);
    let end_row = (app.row_offset + visible_rows).min(row_count);
    let hidden_left = app.col_offset;
    let hidden_right = column_count - end_column;
    let hidden_top = app.row_offset;
    let hidden_bottom = row_count - end_row;
    let column_style = Style::default().fg(if hidden_left + hidden_right > 0 {
        Color::Yellow
    } else {
        Color::DarkGray
    });
    let row_style = Style::default().fg(if hidden_top + hidden_bottom > 0 {
        Color::Yellow
    } else {
        Color::DarkGray
    });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(format!(" {sheet_name} · 网格 · {header_hint} ")).left_aligned())
        .title(
            Line::from(Span::styled(
                format!(
                    " {} ",
                    hidden_columns_label(hidden_left, hidden_right, column_count)
                ),
                column_style,
            ))
            .right_aligned(),
        )
        .title_bottom(
            Line::from(Span::styled(
                format!(
                    " {} ",
                    hidden_rows_label(hidden_top, hidden_bottom, row_count)
                ),
                row_style,
            ))
            .left_aligned(),
        )
        .title_bottom(Line::from(" Enter 整行 · s/S 工作表 · v 列表 · ? 帮助 ").right_aligned());
    frame.render_widget(block, area);

    let Some(sheet) = app.sheet() else {
        return;
    };

    let mut header = vec![Cell::from(Span::styled(
        "行",
        Style::default().fg(Color::DarkGray),
    ))];
    for column in &sheet.columns[app.col_offset..end_column] {
        let style = Style::default().fg(status_color(column.status));
        header.push(Cell::from(Span::styled(
            format!(
                "{}{}",
                status_marker(column.status),
                clip(&column.label, data_width.saturating_sub(1))
            ),
            style,
        )));
    }

    let mut rows = Vec::new();
    for (display_index, sheet_index) in data_rows[app.row_offset..end_row].iter().enumerate() {
        let row = &sheet.rows[*sheet_index];
        let display_position = app.row_offset + display_index;
        let mut cells = vec![Cell::from(Span::styled(
            row_number(row, header_row),
            Style::default().fg(status_color(row.status)),
        ))];
        for (column_index, cell) in row.cells[app.col_offset..end_column].iter().enumerate() {
            let absolute_column = app.col_offset + column_index;
            let selected = display_position == cursor_position && absolute_column == app.grid_col;
            let flashing = selected
                && app
                    .flash
                    .is_some_and(|(flash_sheet, flash_row, flash_column, at)| {
                        flash_sheet == app.sheet_index
                            && flash_row == *sheet_index
                            && flash_column == absolute_column
                            && at.elapsed() < Duration::from_millis(200)
                    });
            let color = status_color(cell.status);
            let style = if flashing {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(color)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };
            let text = one_line(&cell.display());
            let text = grid_cell_text(&text, selected, data_width);
            cells.push(Cell::from(Span::styled(text, style)));
        }
        rows.push(Row::new(cells));
    }

    let mut widths = vec![Constraint::Length(row_number_width as u16)];
    widths.extend(std::iter::repeat_n(
        Constraint::Length(data_width as u16),
        end_column - app.col_offset,
    ));
    let table = Table::new(rows, widths)
        .header(Row::new(header))
        .column_spacing(1);
    frame.render_widget(table, inner);
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    app.last_visible_list_rows = area.height.saturating_sub(3).max(1) as usize;
    let Some(sheet_name) = app.sheet().map(|sheet| sheet.name.clone()) else {
        return;
    };
    let cells = app.filtered_cells();
    if cells.is_empty() {
        frame.render_widget(
            Paragraph::new("当前筛选没有差异单元格")
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("{sheet_name} · 列表")),
                ),
            area,
        );
        app.list_state.select(None);
        return;
    }
    let selected = app.list_selected.min(cells.len() - 1);
    app.list_selected = selected;
    app.list_state.select(Some(selected));

    let Some(sheet) = app.sheet() else {
        return;
    };

    let rows: Vec<Row> = cells
        .iter()
        .map(|(row_index, cell_index)| {
            let row = &sheet.rows[*row_index];
            let cell = &row.cells[*cell_index];
            Row::new(vec![
                Cell::from(
                    cell.new_address
                        .clone()
                        .or_else(|| cell.old_address.clone())
                        .unwrap_or_default(),
                ),
                Cell::from(status_text(cell.status))
                    .style(Style::default().fg(status_color(cell.status))),
                Cell::from(clip(&one_line(&value_text(cell.old_value.as_ref())), 18)),
                Cell::from(clip(&one_line(&value_text(cell.new_value.as_ref())), 18)),
                Cell::from(if cell.formula_changed { "变化" } else { "" }),
                Cell::from(if cell.type_changed { "变化" } else { "" }),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(6),
            Constraint::Length(6),
        ],
    )
    .header(
        Row::new(["地址", "状态", "旧值", "新值", "公式", "类型"])
            .style(Style::default().fg(Color::DarkGray)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!("{} · 列表 · v 网格", sheet.name)),
    )
    .row_highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(table, area, &mut app.list_state);
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let Some(sheet) = app.sheet() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title("单元格详情");
        frame.render_widget(block, area);
        return;
    };
    let cursor = match app.view {
        ViewMode::Grid => Some((app.grid_row, app.grid_col)),
        ViewMode::List => app.filtered_cells().get(app.list_selected).copied(),
    };
    let Some((row_index, cell_index)) = cursor else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title("单元格详情");
        frame.render_widget(
            Paragraph::new("没有单元格")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };
    let Some(row) = sheet.rows.get(row_index) else {
        return;
    };
    let Some(cell) = row.cells.get(cell_index) else {
        return;
    };
    let title = if sheet.header_row.is_some() && row_index == 0 {
        "单元格详情 · 表头行".to_string()
    } else if sheet.header_row.is_some_and(|header| row.number() < header) {
        "单元格详情 · 表前标题".to_string()
    } else {
        "单元格详情".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let address = cell
        .new_address
        .clone()
        .or_else(|| cell.old_address.clone())
        .unwrap_or_else(|| "?".to_string());
    let mut lines = vec![Line::from(vec![Span::styled(
        format!(
            "{} · {} · {}",
            sheet.name,
            address,
            status_text(cell.status)
        ),
        Style::default()
            .fg(status_color(cell.status))
            .add_modifier(Modifier::BOLD),
    )])];
    if let Some(old) = &cell.old_address {
        lines.push(Line::from(vec![
            Span::styled("旧地址  ", Style::default().fg(Color::DarkGray)),
            Span::raw(old.clone()),
        ]));
    }
    if let Some(new) = &cell.new_address {
        lines.push(Line::from(vec![
            Span::styled("新地址  ", Style::default().fg(Color::DarkGray)),
            Span::raw(new.clone()),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("旧值    ", Style::default().fg(Color::DarkGray)),
        Span::raw(clip(&one_line(&value_text(cell.old_value.as_ref())), 80)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("新值    ", Style::default().fg(Color::DarkGray)),
        Span::raw(clip(&one_line(&value_text(cell.new_value.as_ref())), 80)),
    ]));
    if cell.old_formula.is_some() || cell.new_formula.is_some() {
        lines.push(Line::from(vec![
            Span::styled("公式    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "{} → {}{}",
                cell.old_formula.as_deref().unwrap_or("—"),
                cell.new_formula.as_deref().unwrap_or("—"),
                if cell.formula_changed {
                    "（变化）"
                } else {
                    ""
                }
            )),
        ]));
    }
    if cell.type_changed {
        lines.push(Line::from(Span::styled(
            format!(
                "类型     {} → {}",
                cell.old_value
                    .as_ref()
                    .map(CellValue::type_name)
                    .unwrap_or("empty"),
                cell.new_value
                    .as_ref()
                    .map(CellValue::type_name)
                    .unwrap_or("empty")
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    if let Some(note) = &cell.note {
        lines.push(Line::from(Span::styled(
            format!("备注    {note}"),
            Style::default().fg(Color::Cyan),
        )));
    }
    if let Some(diff) = &cell.diff {
        let text = diff
            .new
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        lines.push(Line::from(vec![
            Span::styled("文本    ", Style::default().fg(Color::DarkGray)),
            Span::raw(clip(&one_line(&text), 80)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_row_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(row_index) = app.row_detail else {
        return;
    };
    let data = app.sheet().and_then(|sheet| {
        let row = sheet.rows.get(row_index)?;
        Some((
            sheet.name.clone(),
            row_position_text(row, sheet),
            row_detail_lines(sheet, row_index),
        ))
    });
    let Some((sheet_name, position, lines)) = data else {
        app.row_detail = None;
        return;
    };

    let rect = super::overlay_rect(frame, centered_rect(84, 76, area));
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(vec![
            Span::styled(
                "整行对比",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {sheet_name} · {position}"),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    let inner = block.inner(rect);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width);
    let max = total
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    let scroll = app.row_detail_scroll.min(max);
    let hint = row_detail_scroll_hint(scroll, max);
    let block = if hint.is_empty() {
        block
    } else {
        block.title_bottom(
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))).right_aligned(),
        )
    };
    frame.render_widget(paragraph.block(block).scroll((scroll, 0)), rect);
    app.row_detail_scroll = scroll;
    app.row_detail_max = max;
    app.row_detail_page = inner.height;
}

/// 内容超出浮层时在右下角提示还能往哪边滚。
fn row_detail_scroll_hint(scroll: u16, max: u16) -> String {
    if max == 0 {
        return String::new();
    }
    let below = max.saturating_sub(scroll);
    let mut parts = Vec::new();
    if scroll > 0 {
        parts.push(format!("↑ 上方还有 {scroll} 行"));
    }
    if below > 0 {
        parts.push(format!("↓ 下方还有 {below} 行"));
    }
    format!(" {} ", parts.join(" · "))
}

fn row_detail_lines(sheet: &SheetDiff, row_index: usize) -> Vec<Line<'static>> {
    let Some(row) = sheet.rows.get(row_index) else {
        return Vec::new();
    };
    let changes = row
        .cells
        .iter()
        .filter(|cell| is_visible_change(cell))
        .count();
    let change_text = if changes == 0 {
        " · 本行无变化".to_string()
    } else {
        format!(" · 本行 {changes} 处变化")
    };
    // 行本身未变、但新增/删除列带来了单元格差异时，不能写“未变”。
    let (label, color) = if row.status == Status::Unchanged && changes > 0 {
        ("有变化", Color::Yellow)
    } else {
        (status_text(row.status), status_color(row.status))
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{} ", status_marker(row.status)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{label} · {}", row_position_text(row, sheet)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(change_text, Style::default().fg(Color::DarkGray)),
    ])];
    lines.push(Line::from(""));
    for (index, column) in sheet.columns.iter().enumerate() {
        let body = row_detail_cell_lines(row.cells.get(index));
        lines.push(Line::from(Span::styled(
            format!(
                "{}{}",
                row_detail_column_marker(column.status),
                column.label
            ),
            row_detail_column_style(column.status),
        )));
        lines.extend(body);
    }
    if lines.len() == 2 {
        lines.push(Line::from(Span::styled(
            "本行没有可显示的单元格",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

fn row_detail_cell_lines(cell: Option<&GridCell>) -> Vec<Line<'static>> {
    let Some(cell) = cell else {
        return vec![detail_line("", "（无此列）", Color::DarkGray)];
    };
    let old_text = cell.old_value.as_ref().map(detail_text).unwrap_or_default();
    let new_text = cell.new_value.as_ref().map(detail_text).unwrap_or_default();
    let mut lines = Vec::new();
    if old_text == new_text {
        lines.push(detail_line("值：", &old_text, Color::Reset));
    } else {
        match cell.status {
            Status::Added => lines.push(detail_pair_line("新：", &new_text, Color::Green)),
            Status::Removed => lines.push(detail_pair_line("旧：", &old_text, Color::Red)),
            _ => {
                lines.push(detail_pair_line("旧：", &old_text, Color::Red));
                lines.push(detail_pair_line("新：", &new_text, Color::Green));
            }
        }
    }
    if cell.formula_changed {
        let old_formula = cell.old_formula.as_deref().unwrap_or("");
        let new_formula = cell.new_formula.as_deref().unwrap_or("");
        let text = match (old_formula.is_empty(), new_formula.is_empty()) {
            (true, false) => new_formula.to_string(),
            (false, true) => old_formula.to_string(),
            _ => format!("{old_formula} → {new_formula}"),
        };
        lines.push(detail_line("公式：", &text, Color::Reset));
    } else if lines.is_empty()
        && let Some(formula) = cell.new_formula.as_deref().or(cell.old_formula.as_deref())
    {
        lines.push(detail_line("公式：", formula, Color::Reset));
    }
    if cell.type_changed {
        let old_type = cell
            .old_value
            .as_ref()
            .map(CellValue::type_name)
            .unwrap_or("empty");
        let new_type = cell
            .new_value
            .as_ref()
            .map(CellValue::type_name)
            .unwrap_or("empty");
        lines.push(detail_line(
            "类型：",
            &format!("{old_type} → {new_type}"),
            Color::Yellow,
        ));
    }
    if let Some(note) = &cell.note {
        lines.push(detail_line("备注：", note, Color::Cyan));
    }
    lines
}

/// 旧值/新值行：标签和值都用状态色，旧红新绿，便于对照。
fn detail_pair_line(label: &str, text: &str, color: Color) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            label.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if text.is_empty() {
        spans.push(Span::styled("（空）", Style::default().fg(Color::DarkGray)));
    } else {
        spans.push(Span::styled(text.to_string(), Style::default().fg(color)));
    }
    Line::from(spans)
}

fn detail_line(label: &str, text: &str, color: Color) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    if !label.is_empty() {
        spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if text.is_empty() {
        spans.push(Span::styled("（空）", Style::default().fg(Color::DarkGray)));
    } else {
        spans.push(Span::styled(text.to_string(), Style::default().fg(color)));
    }
    Line::from(spans)
}

fn detail_text(value: &CellValue) -> String {
    one_line(&value.display()).replace('\t', " ")
}

fn row_position_text(row: &GridRow, sheet: &SheetDiff) -> String {
    if sheet.header_row.is_some_and(|header| row.number() < header) {
        return format!("表前第 {} 行", row.number());
    }
    match (row.old_row, row.new_row) {
        (Some(old), Some(new)) if old == new => format!("第 {new} 行"),
        (Some(old), Some(new)) => format!("旧 #{old} → 新 #{new}"),
        (Some(old), None) => format!("旧 #{old}"),
        (None, Some(new)) => format!("新 #{new}"),
        (None, None) => "行号未知".to_string(),
    }
}

fn row_detail_column_marker(status: Status) -> &'static str {
    match status {
        Status::Added => "+",
        Status::Removed => "-",
        Status::Moved => "↔",
        _ => "",
    }
}

fn row_detail_column_style(status: Status) -> Style {
    let color = match status {
        Status::Added => Color::Green,
        Status::Removed => Color::Red,
        Status::Moved => Color::Cyan,
        _ => Color::White,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
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

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let keys = if app.row_detail.is_some() {
        "焦点：整行对比   ↑↓/jk 滚动   PgUp/PgDn 翻页   d/u 半页   g/G 首尾   q/Esc/Enter 关闭   ? 帮助   Ctrl+C 退出"
    } else {
        match app.view {
            ViewMode::Grid => {
                "hjkl 移动   Enter 整行   n/N 变化   ]/[ 行   }/{ 列   f/b 整页   d/u 半页   Tab 筛选   q 退出"
            }
            ViewMode::List => {
                "↑↓ 选择   n/N 变化   Enter 定位   f/b 整页   d/u 半页   Tab 筛选   s/S 表   v 网格   ? 帮助   q 退出"
            }
        }
    };
    let mut lines = vec![Line::from(Span::styled(
        keys,
        Style::default().fg(Color::DarkGray),
    ))];
    if let Some(notice) = active_notice(app) {
        lines.push(Line::from(Span::styled(
            notice.to_string(),
            Style::default().fg(Color::Yellow),
        )));
    } else if app.row_detail.is_some() {
        lines.push(Line::from(Span::styled(
            "整行对比：未变单元格只显示一次，空值标（空），变化单元格显示旧值/新值；不含修订标记",
            Style::default().fg(Color::DarkGray),
        )));
    } else if app.view == ViewMode::List {
        lines.push(Line::from(Span::styled(
            current_filter_line(app),
            Style::default().fg(Color::DarkGray),
        )));
    } else if app.output().is_some() {
        lines.push(Line::from(Span::styled(
            grid_status_line(app),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn active_notice(app: &App) -> Option<&str> {
    let (text, at) = app.notice.as_ref()?;
    (at.elapsed() < Duration::from_secs(3)).then_some(text.as_str())
}

fn grid_status_line(app: &App) -> String {
    let Some(sheet) = app.sheet() else {
        return "—".to_string();
    };
    let data_rows = app.data_row_indices();
    let row_total = data_rows.len();
    let position_in_data = data_rows
        .iter()
        .position(|index| *index == app.grid_row)
        .map(|position| position + 1)
        .unwrap_or(0);
    let column_total = sheet.columns.len();
    let position = grid_position(app).unwrap_or_else(|| "—".to_string());
    let filter_count = app.filter_count(app.filter);
    let mut text = format!(
        "位置 {position} · {} · 第 {}/{} 行 · 第 {}/{} 列 · 工作表 {}/{} · 筛选 {}({})",
        sheet.name,
        position_in_data,
        row_total,
        app.grid_col + 1,
        column_total,
        app.sheet_index + 1,
        app.sheet_count(),
        app.filter.label(),
        filter_count
    );
    if filter_count != app.changes.len() {
        text.push_str(&format!(" · 可跳转 {}", app.changes.len()));
    }
    if let Some(index) = app.current_change_index()
        && let Some(cell) = app.current_cell()
    {
        text.push_str(&format!(
            " · 变化 {}/{} · {}",
            index + 1,
            app.changes.len(),
            status_text(cell.status)
        ));
        let summary = change_value_summary(cell);
        if !summary.is_empty() {
            text.push_str(&format!(" · {summary}"));
        }
    }
    text
}

fn change_value_summary(cell: &GridCell) -> String {
    let old = clip(&one_line(&value_text(cell.old_value.as_ref())), 12);
    let new = clip(&one_line(&value_text(cell.new_value.as_ref())), 12);
    match (old.is_empty(), new.is_empty()) {
        (false, false) if old == new => "值相同".to_string(),
        (false, false) => format!("旧 {old} → 新 {new}"),
        (true, false) => format!("新 {new}"),
        (false, true) => format!("旧 {old}"),
        (true, true) => String::new(),
    }
}

fn current_filter_line(app: &App) -> String {
    Filter::ORDER
        .iter()
        .map(|filter| {
            let marker = if *filter == app.filter { "▸" } else { " " };
            format!("{marker}{}({})", filter.label(), app.filter_count(*filter))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = 74.min(area.width);
    let height = 29.min(area.height);
    let rect = super::overlay_rect(
        frame,
        Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        },
    );
    frame.render_widget(Clear, rect);
    let lines = vec![
        Line::from(Span::styled(
            "Excel 比较 · 键盘说明",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("网格视图", Style::default().fg(Color::Yellow))),
        Line::from("hjkl / 方向键 移动单元格   f/b 或 PgUp/PgDn 整页"),
        Line::from("d/u 或 Ctrl+d/Ctrl+u 半页   g/G 或 Home/End 首尾"),
        Line::from("n/N 变化单元格   Enter 打开整行对比"),
        Line::from("] / [ 变化行   } / { 变化列   S/s 工作表   v 列表"),
        Line::from("Tab / Shift+Tab 切换筛选   1-6 直选"),
        Line::from("空值聚焦显示“·”；跳转后短暂反白并居中"),
        Line::from("表头行自动识别，网格从表头下一行开始"),
        Line::from("表头差异在列表和列头查看   Enter 不跳转表头行"),
        Line::from("边框黄色提示上下左右还有被隐藏的行列"),
        Line::from(""),
        Line::from(Span::styled("整行对比", Style::default().fg(Color::Yellow))),
        Line::from("↑↓/jk 滚动   PgUp/PgDn 翻页   d/u 半页   g/G 首尾"),
        Line::from("未变单元格显示一次，空值标（空）；变化显示旧值(红)/新值(绿)"),
        Line::from("公式变化、类型变化和备注单独标注；不含修订标记"),
        Line::from("q / Esc / Enter 关闭，回到网格"),
        Line::from(""),
        Line::from(Span::styled("列表视图", Style::default().fg(Color::Yellow))),
        Line::from("↑↓ / n/N 选择差异单元格   Enter 定位到网格"),
        Line::from("f/b 整页   d/u 半页   g/G 首行/末行"),
        Line::from("Tab 切换筛选   ]/[ 按行   }/{ 按列   S 跨表找变化"),
        Line::from(Span::styled("说明", Style::default().fg(Color::Yellow))),
        Line::from("首版比较值、公式和工作表结构，不求值重算"),
        Line::from("不比较样式、图表、批注和条件格式；合并单元格取左上角"),
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

fn hidden_columns_label(hidden_left: usize, hidden_right: usize, total: usize) -> String {
    if hidden_left == 0 && hidden_right == 0 {
        return format!("全部 {total} 列");
    }
    let mut parts = Vec::new();
    if hidden_left > 0 {
        parts.push(format!("左侧还有 {hidden_left} 列"));
    }
    if hidden_right > 0 {
        parts.push(format!("右侧还有 {hidden_right} 列"));
    }
    let mut text = parts.join(" · ");
    if hidden_left > 0 {
        text.insert_str(0, "← ");
    }
    if hidden_right > 0 {
        text.push_str(" →");
    }
    text
}

fn hidden_rows_label(hidden_top: usize, hidden_bottom: usize, total: usize) -> String {
    if hidden_top == 0 && hidden_bottom == 0 {
        return format!("全部 {total} 行");
    }
    let mut parts = Vec::new();
    if hidden_top > 0 {
        parts.push(format!("上方还有 {hidden_top} 行"));
    }
    if hidden_bottom > 0 {
        parts.push(format!("下方还有 {hidden_bottom} 行"));
    }
    let mut text = parts.join(" · ");
    if hidden_top > 0 {
        text.insert_str(0, "↑ ");
    }
    if hidden_bottom > 0 {
        text.push_str(" ↓");
    }
    text
}

fn grid_position(app: &App) -> Option<String> {
    let sheet = app.sheet()?;
    let cell = sheet.rows.get(app.grid_row)?.cells.get(app.grid_col)?;
    cell.new_address
        .clone()
        .or_else(|| cell.old_address.clone())
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::Unchanged => "未变",
        Status::Added => "新增",
        Status::Modified => "修改",
        Status::Removed => "删除",
        Status::Moved => "移动",
        Status::SuspectedModified => "疑似",
    }
}

fn is_visible_change(cell: &GridCell) -> bool {
    cell.status != Status::Unchanged && !(cell.status == Status::Moved && cell.display().is_empty())
}

fn status_color(status: Status) -> Color {
    match status {
        Status::Unchanged => Color::Gray,
        Status::Modified | Status::SuspectedModified => Color::Yellow,
        Status::Added => Color::Green,
        Status::Removed => Color::Red,
        Status::Moved => Color::Cyan,
    }
}

fn status_marker(status: Status) -> &'static str {
    match status {
        Status::Unchanged => "",
        Status::Added => "+",
        Status::Removed => "-",
        Status::Modified | Status::SuspectedModified => "*",
        Status::Moved => "↔",
    }
}

fn row_number(row: &GridRow, header_row: Option<usize>) -> String {
    if header_row.is_some_and(|header| row.number() < header) {
        return "表前".to_string();
    }
    match row.status {
        Status::Added => format!("+{}", row.number()),
        Status::Removed => format!("-{}", row.number()),
        Status::Moved => format!("↕{}", row.number()),
        _ => row.number().to_string(),
    }
}

fn header_row_label(header_row: Option<usize>) -> String {
    match header_row {
        Some(row) => format!("表头第 {row} 行"),
        None => "无表头".to_string(),
    }
}

fn value_text(value: Option<&CellValue>) -> String {
    value.map(CellValue::display).unwrap_or_default()
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("?")
        .to_string()
}

fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

fn grid_cell_text(text: &str, selected: bool, width: usize) -> String {
    if selected && text.is_empty() {
        "·".to_string()
    } else {
        clip(text, width)
    }
}

fn clip(text: &str, limit: usize) -> String {
    if UnicodeWidthStr::width(text) <= limit {
        return text.to_string();
    }
    let budget = limit.saturating_sub(1);
    let mut value = String::new();
    let mut width = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > budget {
            break;
        }
        width += character_width;
        value.push(character);
    }
    value.push('…');
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xlsx::result::{ColumnInfo, XlsxSummary};

    #[test]
    fn hidden_labels_name_each_direction() {
        assert_eq!(hidden_columns_label(0, 0, 12), "全部 12 列");
        assert_eq!(hidden_columns_label(3, 0, 12), "← 左侧还有 3 列");
        assert_eq!(hidden_columns_label(0, 4, 12), "右侧还有 4 列 →");
        assert_eq!(
            hidden_columns_label(2, 4, 12),
            "← 左侧还有 2 列 · 右侧还有 4 列 →"
        );
        assert_eq!(hidden_rows_label(0, 0, 28), "全部 28 行");
        assert_eq!(hidden_rows_label(5, 0, 28), "↑ 上方还有 5 行");
        assert_eq!(hidden_rows_label(0, 6, 28), "下方还有 6 行 ↓");
        assert_eq!(
            hidden_rows_label(5, 6, 28),
            "↑ 上方还有 5 行 · 下方还有 6 行 ↓"
        );
    }

    #[test]
    fn clips_long_cell_text_with_ellipsis() {
        assert_eq!(clip("短文本", 12), "短文本");
        assert_eq!(clip("0123456789ab", 12), "0123456789ab");
        assert_eq!(clip("0123456789abc", 12), "0123456789a…");
        assert_eq!(clip("借调至省公司安监部挂职", 12), "借调至省公…");
    }

    #[test]
    fn labels_title_rows_before_header() {
        let title = GridRow {
            old_row: Some(1),
            new_row: Some(1),
            status: Status::Unchanged,
            cells: Vec::new(),
        };
        let data = GridRow {
            old_row: Some(3),
            new_row: Some(3),
            status: Status::Moved,
            cells: Vec::new(),
        };
        assert_eq!(row_number(&title, Some(2)), "表前");
        assert_eq!(row_number(&data, Some(2)), "↕3");
        assert_eq!(header_row_label(Some(2)), "表头第 2 行");
        assert_eq!(header_row_label(None), "无表头");
    }

    #[test]
    fn marks_the_focused_empty_cell() {
        assert_eq!(grid_cell_text("", true, 12), "·");
        assert_eq!(grid_cell_text("", false, 12), "");
        assert_eq!(grid_cell_text("张三", true, 12), "张三");
    }

    fn test_cell(status: Status) -> GridCell {
        GridCell {
            status,
            old_col: None,
            new_col: None,
            old_address: None,
            new_address: None,
            old_value: None,
            new_value: None,
            old_formula: None,
            new_formula: None,
            value_changed: false,
            formula_changed: false,
            type_changed: false,
            diff: None,
            note: None,
        }
    }

    fn test_row(number: usize, statuses: &[Status]) -> GridRow {
        GridRow {
            old_row: Some(number),
            new_row: Some(number),
            status: Status::Unchanged,
            cells: statuses.iter().copied().map(test_cell).collect(),
        }
    }

    fn value_cell(status: Status, old: Option<&str>, new: Option<&str>) -> GridCell {
        let mut cell = test_cell(status);
        cell.old_value = old.map(|text| CellValue::Text(text.to_string()));
        cell.new_value = new.map(|text| CellValue::Text(text.to_string()));
        cell.value_changed = old != new;
        cell
    }

    fn detail_sheet() -> SheetDiff {
        let columns = vec![
            ColumnInfo {
                label: "名称".to_string(),
                old_col: Some(1),
                new_col: Some(1),
                status: Status::Unchanged,
            },
            ColumnInfo {
                label: "数量".to_string(),
                old_col: Some(2),
                new_col: Some(2),
                status: Status::Unchanged,
            },
            ColumnInfo {
                label: "新增列".to_string(),
                old_col: None,
                new_col: Some(3),
                status: Status::Added,
            },
            ColumnInfo {
                label: "删除列".to_string(),
                old_col: Some(4),
                new_col: None,
                status: Status::Removed,
            },
            ColumnInfo {
                label: "空列".to_string(),
                old_col: Some(5),
                new_col: Some(5),
                status: Status::Unchanged,
            },
        ];
        SheetDiff {
            name: "指标".to_string(),
            status: Status::Modified,
            header_row: Some(1),
            columns,
            rows: vec![
                test_row(1, &[Status::Unchanged; 5]),
                GridRow {
                    old_row: Some(2),
                    new_row: Some(2),
                    status: Status::Modified,
                    cells: vec![
                        value_cell(Status::Unchanged, Some("售电量"), Some("售电量")),
                        value_cell(Status::Modified, Some("100"), Some("120")),
                        value_cell(Status::Added, None, Some("备注 A")),
                        value_cell(Status::Removed, Some("旧备注"), None),
                        test_cell(Status::Unchanged),
                    ],
                },
            ],
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn row_detail_shows_unchanged_once_and_old_new_for_changes() {
        let sheet = detail_sheet();
        let lines = row_detail_lines(&sheet, 1);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("修改 · 第 2 行"), "{text}");
        assert_eq!(
            text.matches("售电量").count(),
            1,
            "未变单元格只显示一次: {text}"
        );
        assert!(text.contains("旧：100"), "{text}");
        assert!(text.contains("新：120"), "{text}");
        assert!(text.contains("新：备注 A"), "{text}");
        assert!(!text.contains("旧：备注 A"), "{text}");
        assert!(text.contains("旧：旧备注"), "{text}");
        assert!(!text.contains("新：旧备注"), "{text}");
        assert!(text.contains("空列"), "空值列也要列出: {text}");
        assert!(text.contains("值：（空）"), "空值应标注（空）: {text}");
        for label in ["名称", "数量", "新增列", "删除列", "空列"] {
            assert!(text.contains(label), "缺少列 {label}: {text}");
        }
        let span_style = |needle: &str| -> Style {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.as_ref() == needle)
                .unwrap_or_else(|| panic!("找不到 {needle}: {text}"))
                .style
        };
        for (label, color) in [("旧：", Color::Red), ("新：", Color::Green)] {
            let style = span_style(label);
            assert!(
                style.add_modifier.contains(Modifier::BOLD),
                "{label} 应加粗"
            );
            assert_eq!(style.fg, Some(color), "{label} 颜色");
        }
        assert_eq!(span_style("100").fg, Some(Color::Red), "旧值应红色");
        assert_eq!(span_style("120").fg, Some(Color::Green), "新值应绿色");
        assert!(
            lines.iter().flat_map(|line| &line.spans).all(|span| !span
                .style
                .add_modifier
                .intersects(Modifier::CROSSED_OUT | Modifier::UNDERLINED)),
            "整行对比不显示修订标记: {text}"
        );
    }

    #[test]
    fn row_detail_scroll_hint_points_to_hidden_content() {
        assert_eq!(row_detail_scroll_hint(0, 0), "");
        assert_eq!(row_detail_scroll_hint(0, 6), " ↓ 下方还有 6 行 ");
        assert_eq!(row_detail_scroll_hint(6, 6), " ↑ 上方还有 6 行 ");
        assert_eq!(
            row_detail_scroll_hint(2, 5),
            " ↑ 上方还有 2 行 · ↓ 下方还有 3 行 "
        );
    }

    #[test]
    fn row_detail_does_not_claim_unchanged_when_only_columns_changed() {
        let mut sheet = detail_sheet();
        sheet.rows[1].status = Status::Unchanged;
        let text = row_detail_lines(&sheet, 1)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(text.contains("有变化 · 第 2 行"), "{text}");
        assert!(!text.contains("未变 · 第 2 行"), "{text}");
        assert!(text.contains("+新增列"), "{text}");
        assert!(text.contains("-删除列"), "{text}");
    }

    #[test]
    fn row_detail_lists_formula_and_type_changes() {
        let mut sheet = detail_sheet();
        let cell = &mut sheet.rows[1].cells[1];
        cell.old_value = Some(CellValue::Integer(100));
        cell.new_value = Some(CellValue::Float(120.0));
        cell.old_formula = Some("=B2".to_string());
        cell.new_formula = Some("=B2*1.2".to_string());
        cell.formula_changed = true;
        cell.type_changed = true;
        let text = row_detail_lines(&sheet, 1)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("公式：=B2 → =B2*1.2"), "{text}");
        assert!(text.contains("类型：integer → float"), "{text}");
    }

    #[test]
    fn row_detail_opens_from_grid_and_closes_without_quitting() {
        let mut app = navigation_app();
        let press = |app: &mut App, code| {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        };

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.row_detail, Some(1));

        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.row_detail, None);
        assert!(!app.quit, "整行对比里的 q 不应退出程序");

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.row_detail, None);

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.row_detail, None, "Enter 也应关闭整行对比");

        let mut empty = test_app(vec![test_sheet(
            "S",
            Some(1),
            vec![test_row(1, &[Status::Unchanged, Status::Unchanged])],
        )]);
        empty.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(empty.row_detail, None, "没有数据行时不应打开整行对比");

        app.toggle_view();
        assert_eq!(app.view, ViewMode::List);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.row_detail, None, "列表里的 Enter 仍是定位");
        assert_eq!(app.view, ViewMode::Grid);
    }

    #[test]
    fn row_detail_scroll_keys_clamp_to_content() {
        let mut app = navigation_app();
        let press = |app: &mut App, code| {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        };
        press(&mut app, KeyCode::Enter);
        app.row_detail_page = 5;
        app.row_detail_max = 20;

        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.row_detail_scroll, 1);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.row_detail_scroll, 6);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.row_detail_scroll, 20);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.row_detail_scroll, 20);
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.row_detail_scroll, 0);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.row_detail_scroll, 0);

        press(&mut app, KeyCode::Char('?'));
        assert!(app.help);
        assert_eq!(app.row_detail, Some(1), "帮助关闭前不应丢掉整行对比");
    }

    #[test]
    fn row_detail_draws_overlay_and_remembers_scroll_page() {
        use ratatui::backend::TestBackend;

        let mut app = test_app(vec![detail_sheet()]);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
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
        assert!(compact.contains("整行对比"), "{screen}");
        assert!(compact.contains("旧：100"), "{screen}");
        assert!(app.row_detail_page > 0);
        assert_eq!(app.last_overlay, Overlay::RowDetail);
    }

    fn test_sheet(name: &str, header_row: Option<usize>, rows: Vec<GridRow>) -> SheetDiff {
        let widest = rows.iter().map(|row| row.cells.len()).max().unwrap_or(0);
        let columns = (1..=widest)
            .map(|column| ColumnInfo {
                label: crate::xlsx::model::column_label(column),
                old_col: Some(column),
                new_col: Some(column),
                status: Status::Unchanged,
            })
            .collect();
        SheetDiff {
            name: name.to_string(),
            status: Status::Modified,
            header_row,
            columns,
            rows,
        }
    }

    fn test_app(sheets: Vec<SheetDiff>) -> App {
        let mut app = App::new(PathBuf::from("old.xlsx"), PathBuf::from("new.xlsx"));
        app.state = State::Ready(Box::new(XlsxOutput {
            old_path: PathBuf::from("old.xlsx"),
            new_path: PathBuf::from("new.xlsx"),
            old_sheet_count: sheets.len(),
            new_sheet_count: sheets.len(),
            sheets,
            summary: XlsxSummary::default(),
            elapsed: Duration::ZERO,
            note: None,
        }));
        app.rebuild_changes();
        app.normalize_grid_row();
        app.reset_change_anchor = false;
        app
    }

    fn navigation_app() -> App {
        test_app(vec![test_sheet(
            "S",
            Some(1),
            vec![
                test_row(
                    1,
                    &[Status::Unchanged, Status::Unchanged, Status::Unchanged],
                ),
                test_row(2, &[Status::Modified, Status::Unchanged, Status::Added]),
                test_row(3, &[Status::Unchanged, Status::Removed, Status::Unchanged]),
            ],
        )])
    }

    #[test]
    fn filter_keys_work_in_grid_view() {
        let mut app = navigation_app();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.filter, Filter::Modified);
        assert_eq!(app.changes, vec![(1, 0)]);

        app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(app.filter, Filter::Added);
        assert_eq!(app.changes, vec![(1, 2)]);

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(app.filter, Filter::Suspected);
        assert!(app.changes.is_empty());
        assert!(grid_status_line(&app).contains("筛选 疑似(0)"));
    }

    #[test]
    fn page_keys_follow_less_in_both_views() {
        let mut rows = vec![test_row(1, &[Status::Unchanged])];
        for number in 2..=11 {
            rows.push(test_row(number, &[Status::Modified]));
        }
        let mut app = test_app(vec![test_sheet("S", Some(1), rows)]);
        app.last_visible_rows = 4;
        app.last_visible_list_rows = 4;

        let press = |app: &mut App, code| {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        };

        press(&mut app, KeyCode::Char('f'));
        assert_eq!(app.grid_row, 5);
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.grid_row, 1);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.grid_row, 3);
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.grid_row, 1);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.grid_row, 5);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.grid_row, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.grid_row, 3);
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.grid_row, 1);

        app.toggle_view();
        assert_eq!(app.view, ViewMode::List);
        app.list_selected = 0;
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(app.list_selected, 4);
        assert_eq!(app.filter, Filter::All);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.list_selected, 6);
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.list_selected, 2);
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.list_selected, 0);
    }

    #[test]
    fn switching_to_list_selects_the_nearest_change() {
        let mut app = navigation_app();
        app.grid_row = 1;
        app.grid_col = 1;
        app.toggle_view();
        assert_eq!(app.view, ViewMode::List);
        assert_eq!(app.list_selected, 1);
        app.toggle_view();
        assert_eq!(app.view, ViewMode::Grid);
        assert_eq!((app.grid_row, app.grid_col), (1, 2));
    }

    #[test]
    fn skips_empty_moved_cells_in_navigation() {
        let app = test_app(vec![test_sheet(
            "S",
            Some(1),
            vec![
                test_row(1, &[Status::Unchanged, Status::Unchanged]),
                test_row(2, &[Status::Moved, Status::Modified]),
            ],
        )]);
        assert_eq!(app.changes, vec![(1, 1)]);
    }

    #[test]
    fn change_navigation_wraps_and_follows_the_filter() {
        let mut app = navigation_app();
        assert_eq!(app.changes, vec![(1, 0), (1, 2), (2, 1)]);
        let sheet = app.sheet().unwrap();
        let changed = sheet
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .filter(|cell| cell.status != Status::Unchanged)
            .count();
        assert_eq!(changed, 3);

        app.grid_row = 1;
        app.grid_col = 0;
        app.next_change(true);
        assert_eq!((app.grid_row, app.grid_col), (1, 2));
        app.next_change(true);
        assert_eq!((app.grid_row, app.grid_col), (2, 1));
        app.next_change(true);
        assert_eq!((app.grid_row, app.grid_col), (1, 0));
        assert_eq!(active_notice(&app), Some("已回到第一个变化"));

        app.next_change(false);
        assert_eq!((app.grid_row, app.grid_col), (2, 1));
        assert_eq!(active_notice(&app), Some("已到最后一个变化"));

        app.set_filter(Filter::Removed);
        assert_eq!(app.changes, vec![(2, 1)]);
        assert_eq!(active_notice(&app), None);
        app.next_change(true);
        assert_eq!((app.grid_row, app.grid_col), (2, 1));
    }

    #[test]
    fn changed_row_navigation_keeps_the_column() {
        let mut app = test_app(vec![test_sheet(
            "S",
            Some(1),
            vec![
                test_row(
                    1,
                    &[Status::Unchanged, Status::Unchanged, Status::Unchanged],
                ),
                test_row(2, &[Status::Modified, Status::Unchanged, Status::Unchanged]),
                test_row(
                    3,
                    &[Status::Unchanged, Status::Unchanged, Status::Unchanged],
                ),
                test_row(4, &[Status::Unchanged, Status::Unchanged, Status::Added]),
            ],
        )]);
        app.grid_row = 1;
        app.grid_col = 1;
        app.next_changed_row(true);
        assert_eq!((app.grid_row, app.grid_col), (3, 1));
        assert_eq!(active_notice(&app), Some("行 4 有 1 处变化"));

        app.grid_col = 9;
        app.next_changed_row(false);
        assert_eq!((app.grid_row, app.grid_col), (1, 0));
    }

    #[test]
    fn changed_column_navigation_keeps_the_row() {
        let mut app = test_app(vec![test_sheet(
            "S",
            Some(1),
            vec![
                test_row(
                    1,
                    &[Status::Unchanged, Status::Unchanged, Status::Unchanged],
                ),
                test_row(2, &[Status::Modified, Status::Unchanged, Status::Unchanged]),
                test_row(3, &[Status::Unchanged, Status::Unchanged, Status::Added]),
            ],
        )]);
        app.grid_row = 1;
        app.grid_col = 0;
        app.next_changed_column(true);
        assert_eq!((app.grid_row, app.grid_col), (1, 2));
        assert_eq!(active_notice(&app), Some("列 C 有 1 处变化"));

        app.grid_row = 99;
        app.next_changed_column(false);
        assert_eq!((app.grid_row, app.grid_col), (1, 0));
    }

    #[test]
    fn sheet_jump_lands_on_the_first_filtered_change() {
        let mut app = test_app(vec![
            test_sheet(
                "S1",
                Some(1),
                vec![
                    test_row(1, &[Status::Unchanged, Status::Unchanged]),
                    test_row(2, &[Status::Modified, Status::Unchanged]),
                ],
            ),
            test_sheet(
                "S2",
                Some(1),
                vec![
                    test_row(1, &[Status::Unchanged, Status::Unchanged]),
                    test_row(2, &[Status::Unchanged, Status::Added]),
                    test_row(3, &[Status::Removed, Status::Unchanged]),
                ],
            ),
        ]);
        app.jump_to_changed_sheet();
        assert_eq!(app.sheet_index, 1);
        assert_eq!((app.grid_row, app.grid_col), (1, 1));
        assert_eq!(active_notice(&app), Some("工作表 S2 · 可跳转 2"));

        app.set_filter(Filter::Modified);
        app.jump_to_changed_sheet();
        assert_eq!(app.sheet_index, 0);
        assert_eq!((app.grid_row, app.grid_col), (1, 0));
    }

    #[test]
    fn sheet_jump_reports_an_empty_filter() {
        let mut app = navigation_app();
        app.set_filter(Filter::Suspected);
        app.jump_to_changed_sheet();
        assert_eq!(app.sheet_index, 0);
        assert_eq!(active_notice(&app), Some("当前筛选无变化"));
    }
}
