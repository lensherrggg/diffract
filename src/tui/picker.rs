use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pinyin::ToPinyinMulti;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthChar;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Old,
    New,
}

impl Role {
    fn title(self) -> &'static str {
        match self {
            Role::Old => "选择旧版文件",
            Role::New => "选择新版文件",
        }
    }

    fn step(self) -> &'static str {
        match self {
            Role::Old => "第 1/2 步",
            Role::New => "第 2/2 步",
        }
    }

    fn context_label(self) -> &'static str {
        match self {
            Role::Old => "已选新版",
            Role::New => "已选旧版",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Filter,
    Path,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Filter => "筛选",
            Mode::Path => "路径",
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    is_parent: bool,
    modified: Option<SystemTime>,
    /// 每个可匹配字符的首字母候选；多音字保留全部读音。
    initials: Vec<Vec<char>>,
}

impl Entry {
    fn kind_label(&self) -> &'static str {
        if self.is_parent {
            "上级"
        } else if self.is_dir {
            "目录"
        } else {
            "文件"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedQuery {
    dir: PathBuf,
    filter: String,
    mode: Mode,
}

struct ResolvedQuery {
    dir: PathBuf,
    filter: String,
    mode: Mode,
    direct_file: Option<PathBuf>,
    focus_dir: Option<PathBuf>,
    error: Option<String>,
}

enum Action {
    Stay,
    Pick(PathBuf),
    Esc,
    Cancel,
}

enum PickerOutcome {
    Picked(PathBuf),
    Esc,
    Cancel,
}

struct Picker {
    role: Role,
    context: Option<PathBuf>,
    cwd: PathBuf,
    view_dir: PathBuf,
    mode: Mode,
    query: Vec<char>,
    cursor: usize,
    show_all: bool,
    entries: Vec<Entry>,
    listing_dir: Option<PathBuf>,
    filtered: Vec<usize>,
    selected: usize,
    table_state: TableState,
    direct_file: Option<PathBuf>,
    error: Option<String>,
    status: Option<String>,
}

impl Picker {
    fn new(role: Role, start_dir: PathBuf, context: Option<PathBuf>) -> Self {
        let cwd = absolute(start_dir);
        let mut picker = Self {
            role,
            context,
            view_dir: cwd.clone(),
            cwd,
            mode: Mode::Filter,
            query: Vec::new(),
            cursor: 0,
            show_all: false,
            entries: Vec::new(),
            listing_dir: None,
            filtered: Vec::new(),
            selected: 0,
            table_state: TableState::default(),
            direct_file: None,
            error: None,
            status: None,
        };
        picker.refresh();
        picker
    }

    fn refresh(&mut self) {
        self.status = None;
        let query: String = self.query.iter().collect();
        let resolved = resolve_query(&query, &self.cwd);
        self.mode = resolved.mode;
        self.view_dir = resolved.dir.clone();
        self.direct_file = resolved.direct_file.clone();
        self.error = resolved.error;

        if self.error.is_some() {
            self.filtered.clear();
            self.selected = 0;
            self.table_state.select(None);
            return;
        }

        if self.listing_dir.as_deref() != Some(resolved.dir.as_path()) {
            match read_entries(&resolved.dir) {
                Ok(entries) => {
                    self.entries = entries;
                    self.listing_dir = Some(resolved.dir.clone());
                }
                Err(error) => {
                    self.entries.clear();
                    self.listing_dir = Some(resolved.dir.clone());
                    self.filtered.clear();
                    self.selected = 0;
                    self.table_state.select(None);
                    self.error = Some(error);
                    return;
                }
            }
        }

        self.filtered = compute_filtered(
            &self.entries,
            &resolved.filter,
            self.show_all,
            resolved.direct_file.as_deref(),
        );

        if let Some(focus) = resolved.focus_dir.as_deref()
            && let Some(position) = self
                .filtered
                .iter()
                .position(|&index| paths_equal(&self.entries[index].path, focus))
        {
            self.selected = position;
            self.table_state.select(Some(position));
            return;
        }

        if self.filtered.is_empty() {
            self.selected = 0;
            self.table_state.select(None);
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
            self.table_state.select(Some(self.selected));
        }
    }

    fn current_entry(&self) -> Option<&Entry> {
        self.filtered
            .get(self.selected)
            .map(|&index| &self.entries[index])
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let next =
            (self.selected as isize + delta).clamp(0, self.filtered.len() as isize - 1) as usize;
        self.selected = next;
        self.table_state.select(Some(next));
    }

    fn select_first(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = 0;
        self.table_state.select(Some(0));
    }

    fn select_last(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = self.filtered.len() - 1;
        self.table_state.select(Some(self.selected));
    }

    fn set_query(&mut self, text: &str) {
        self.query = text.chars().collect();
        self.cursor = self.query.len();
        self.selected = 0;
        self.refresh();
    }

    fn insert_char(&mut self, ch: char) {
        self.query.insert(self.cursor, ch);
        self.cursor += 1;
        self.selected = 0;
        self.refresh();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.query.remove(self.cursor);
            self.selected = 0;
            self.refresh();
        } else if self.query.is_empty() {
            self.go_parent();
        }
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.query.len() {
            self.query.remove(self.cursor);
            self.selected = 0;
            self.refresh();
        }
    }

    fn clear_query(&mut self) {
        if self.query.is_empty() {
            return;
        }
        self.query.clear();
        self.cursor = 0;
        self.selected = 0;
        self.refresh();
    }

    fn go_parent(&mut self) {
        let Some(parent) = self.cwd.parent() else {
            self.status = Some("已到根目录".to_string());
            return;
        };
        if parent == self.cwd {
            self.status = Some("已到根目录".to_string());
            return;
        }
        let parent = parent.to_path_buf();
        self.navigate(parent);
    }

    fn navigate(&mut self, path: PathBuf) {
        self.cwd = absolute(path);
        self.query.clear();
        self.cursor = 0;
        self.selected = 0;
        self.refresh();
    }

    fn toggle_show_all(&mut self) {
        let keep = self.current_entry().map(|entry| entry.path.clone());
        self.show_all = !self.show_all;
        self.selected = 0;
        self.refresh();
        if let Some(keep) = keep
            && let Some(position) = self
                .filtered
                .iter()
                .position(|&index| paths_equal(&self.entries[index].path, &keep))
        {
            self.selected = position;
            self.table_state.select(Some(position));
        }
    }

    fn complete_selected(&mut self) {
        let Some(entry) = self.current_entry() else {
            return;
        };
        let mut text = entry.path.to_string_lossy().to_string();
        if entry.is_dir && !text.ends_with(std::path::MAIN_SEPARATOR) {
            text.push(std::path::MAIN_SEPARATOR);
        }
        self.set_query(&text);
    }

    fn accept(&mut self) -> Action {
        let Some((path, is_dir, name)) = self
            .current_entry()
            .map(|entry| (entry.path.clone(), entry.is_dir, entry.name.clone()))
        else {
            self.status = Some("没有可选择的文件或目录".to_string());
            return Action::Stay;
        };
        if is_dir {
            self.navigate(path);
            Action::Stay
        } else if is_docx(&name) {
            Action::Pick(path)
        } else {
            self.status = Some("只支持 .docx 文件；Ctrl+T 可显示全部文件".to_string());
            Action::Stay
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::Stay;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return Action::Cancel,
                KeyCode::Char('t') => {
                    self.toggle_show_all();
                    return Action::Stay;
                }
                KeyCode::Char('u') => {
                    self.clear_query();
                    return Action::Stay;
                }
                KeyCode::Char('n') => {
                    self.move_selection(1);
                    return Action::Stay;
                }
                KeyCode::Char('p') => {
                    self.move_selection(-1);
                    return Action::Stay;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => Action::Esc,
            KeyCode::Enter => self.accept(),
            KeyCode::Up => {
                self.move_selection(-1);
                Action::Stay
            }
            KeyCode::Down => {
                self.move_selection(1);
                Action::Stay
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                Action::Stay
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                Action::Stay
            }
            KeyCode::Tab => {
                self.complete_selected();
                Action::Stay
            }
            KeyCode::Backspace => {
                self.backspace();
                Action::Stay
            }
            KeyCode::Delete => {
                self.delete_forward();
                Action::Stay
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::Stay
            }
            KeyCode::Right => {
                if self.cursor < self.query.len() {
                    self.cursor += 1;
                }
                Action::Stay
            }
            KeyCode::Home => {
                if self.query.is_empty() {
                    self.select_first();
                } else {
                    self.cursor = 0;
                }
                Action::Stay
            }
            KeyCode::End => {
                if self.query.is_empty() {
                    self.select_last();
                } else {
                    self.cursor = self.query.len();
                }
                Action::Stay
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_char(ch);
                Action::Stay
            }
            _ => Action::Stay,
        }
    }
}

pub fn pick(
    terminal: &mut TuiTerminal,
    old: Option<PathBuf>,
    new: Option<PathBuf>,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let old_from_cli = old.is_some();
    let mut old = old;
    let mut new = new;
    let mut resume_old_dir = None;

    loop {
        if old.is_none() {
            let start = resume_old_dir
                .take()
                .unwrap_or_else(|| start_dir_for(new.as_deref()));
            match run_picker(terminal, Role::Old, start, new.as_deref())? {
                PickerOutcome::Picked(path) => old = Some(path),
                PickerOutcome::Esc | PickerOutcome::Cancel => return Ok(None),
            }
        }

        if new.is_none() {
            let start = start_dir_for(old.as_deref());
            match run_picker(terminal, Role::New, start, old.as_deref())? {
                PickerOutcome::Picked(path) => {
                    new = Some(path);
                    break;
                }
                PickerOutcome::Cancel => return Ok(None),
                PickerOutcome::Esc => {
                    if old_from_cli {
                        return Ok(None);
                    }
                    resume_old_dir = old.as_deref().and_then(Path::parent).map(Path::to_path_buf);
                    old = None;
                    continue;
                }
            }
        }

        break;
    }

    match (old, new) {
        (Some(old), Some(new)) => Ok(Some((old, new))),
        _ => Ok(None),
    }
}

fn run_picker(
    terminal: &mut TuiTerminal,
    role: Role,
    start_dir: PathBuf,
    context: Option<&Path>,
) -> Result<PickerOutcome> {
    let mut picker = Picker::new(role, start_dir, context.map(Path::to_path_buf));
    loop {
        terminal.draw(|frame| draw_picker(frame, &mut picker))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        match picker.handle_key(key) {
            Action::Stay => {}
            Action::Pick(path) => return Ok(PickerOutcome::Picked(path)),
            Action::Esc => return Ok(PickerOutcome::Esc),
            Action::Cancel => return Ok(PickerOutcome::Cancel),
        }
    }
}

fn draw_picker(frame: &mut Frame, picker: &mut Picker) {
    let chunks = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(frame.area());
    draw_picker_header(frame, chunks[0], picker);
    draw_picker_list(frame, chunks[1], picker);
    draw_picker_footer(frame, chunks[2], picker);
}

fn draw_picker_header(frame: &mut Frame, area: Rect, picker: &Picker) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            " {} · {} ",
            picker.role.title(),
            picker.role.step()
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let mut lines = Vec::with_capacity(4);
    lines.push(Line::from(vec![
        Span::styled("目录  ", Style::default().fg(Color::DarkGray)),
        Span::raw(elide_start(
            &picker.view_dir.display().to_string(),
            width.saturating_sub(6),
        )),
    ]));
    lines.push(match &picker.context {
        Some(context) => Line::from(vec![
            Span::styled(
                format!("{}  ", picker.role.context_label()),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(elide_start(
                &context.display().to_string(),
                width.saturating_sub(8),
            )),
        ]),
        None => Line::from(vec![
            Span::styled("范围  ", Style::default().fg(Color::DarkGray)),
            Span::raw(range_label(picker.show_all)),
        ]),
    });
    lines.push(input_line(picker, width));
    lines.push(if let Some(error) = &picker.error {
        Line::from(Span::styled(
            format!("错误  {error}"),
            Style::default().fg(Color::Red),
        ))
    } else if let Some(status) = &picker.status {
        Line::from(Span::styled(
            format!("提示  {status}"),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        let mut text = format!(
            "模式  {} · {}",
            picker.mode.label(),
            range_label(picker.show_all)
        );
        if picker.direct_file.is_some() {
            text.push_str(" · 已定位到输入的路径");
        }
        if picker.filtered.is_empty() {
            text.push_str(" · 无匹配项");
        }
        Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
    });

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_picker_list(frame: &mut Frame, area: Rect, picker: &mut Picker) {
    let rows: Vec<Row> = picker
        .filtered
        .iter()
        .map(|&index| {
            let entry = &picker.entries[index];
            Row::new(vec![
                Cell::from(entry.kind_label()),
                Cell::from(entry.name.clone()),
                Cell::from(format_modified(entry.modified)),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(if picker.show_all {
            "文件 · 全部"
        } else {
            "文件 · .docx"
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Min(12),
            Constraint::Length(16),
        ],
    )
    .header(Row::new(["类型", "名称", "修改时间"]).style(Style::default().fg(Color::DarkGray)))
    .block(block)
    .row_highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(table, area, &mut picker.table_state);
}

fn draw_picker_footer(frame: &mut Frame, area: Rect, picker: &Picker) {
    let keys = match picker.role {
        Role::New => {
            "↑↓ 选择   Enter 确认   Tab 补全   Backspace 上级   Ctrl+T 全部   Esc 返回上一步"
        }
        Role::Old => "↑↓ 选择   Enter 确认   Tab 补全   Backspace 上级   Ctrl+T 全部   Esc 取消",
    };
    let count = format!(
        "共 {} 项 · {} · {}",
        picker.filtered.len(),
        range_label(picker.show_all),
        if picker.mode == Mode::Path {
            "路径模式"
        } else {
            "筛选模式"
        }
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(keys, Style::default().fg(Color::DarkGray))),
            Line::from(Span::styled(count, Style::default().fg(Color::DarkGray))),
        ]),
        area,
    );
}

fn input_line(picker: &Picker, width: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled("输入  ", Style::default().fg(Color::DarkGray)),
        Span::raw("["),
    ];
    if picker.query.is_empty() {
        spans.push(Span::styled(
            " ",
            Style::default().add_modifier(Modifier::REVERSED),
        ));
        spans.push(Span::styled(
            " 输入文件名、拼音首字母，或 / ~ C:\\ 开头输入路径",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        let available = width.saturating_sub(8).max(4);
        let (start, end) = input_window(picker, available);
        if start > 0 {
            spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
        }
        for (index, ch) in picker.query.iter().enumerate().take(end).skip(start) {
            let style = if index == picker.cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        if picker.cursor == picker.query.len() && end == picker.query.len() {
            spans.push(Span::styled(
                " ",
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        }
        if end < picker.query.len() {
            spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
        }
    }
    spans.push(Span::raw("]"));
    Line::from(spans)
}

fn input_window(picker: &Picker, available: usize) -> (usize, usize) {
    let cursor = picker.cursor.min(picker.query.len());
    let widths: Vec<usize> = picker.query.iter().map(|&ch| char_width(ch)).collect();
    let budget = available.saturating_sub(2);
    let cursor_width = if cursor < picker.query.len() {
        widths[cursor]
    } else {
        1
    };

    let left_budget = if cursor == picker.query.len() {
        budget
    } else {
        (budget.saturating_sub(cursor_width)) / 2
    };
    let mut start = cursor;
    let mut left_used = 0;
    while start > 0 {
        let width = widths[start - 1];
        if left_used + width > left_budget {
            break;
        }
        left_used += width;
        start -= 1;
    }

    let right_budget = budget.saturating_sub(left_used + cursor_width);
    let mut end = cursor;
    let mut right_used = 0;
    if cursor < picker.query.len() {
        end += 1;
    }
    while end < picker.query.len() {
        let width = widths[end];
        if right_used + width > right_budget {
            break;
        }
        right_used += width;
        end += 1;
    }
    (start, end)
}

fn start_dir_for(file: Option<&Path>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(parent) = file
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return cwd;
    };
    let parent = absolute(parent.to_path_buf());
    if parent.is_dir() { parent } else { cwd }
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    cwd.join(path)
}

fn range_label(show_all: bool) -> &'static str {
    if show_all {
        "显示全部文件（Ctrl+T 只看 .docx）"
    } else {
        "仅显示 .docx（Ctrl+T 显示全部）"
    }
}

fn read_entries(dir: &Path) -> std::result::Result<Vec<Entry>, String> {
    let read =
        fs::read_dir(dir).map_err(|err| format!("无法读取目录：{}（{err}）", dir.display()))?;
    let mut entries = Vec::new();

    if let Some(parent) = dir.parent()
        && parent != dir
    {
        entries.push(Entry {
            path: parent.to_path_buf(),
            name: "..".to_string(),
            is_dir: true,
            is_parent: true,
            modified: None,
            initials: Vec::new(),
        });
    }

    for item in read {
        let Ok(item) = item else {
            continue;
        };
        let name = item.file_name().to_string_lossy().to_string();
        let path = item.path();
        let file_type = item.file_type().ok();
        let is_dir = file_type.as_ref().is_some_and(|kind| kind.is_dir());
        let is_symlink = file_type.as_ref().is_some_and(|kind| kind.is_symlink());
        let is_dir =
            is_dir || (is_symlink && fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()));
        let modified = item
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        let initials = initials_of(&name);
        entries.push(Entry {
            path,
            name,
            is_dir,
            is_parent: false,
            modified,
            initials,
        });
    }

    entries.sort_by(|a, b| {
        b.is_parent
            .cmp(&a.is_parent)
            .then_with(|| b.is_dir.cmp(&a.is_dir))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

fn initials_of(name: &str) -> Vec<Vec<char>> {
    let mut initials = Vec::new();
    for ch in name.chars() {
        if let Some(multi) = ch.to_pinyin_multi() {
            let mut options: Vec<char> = multi
                .into_iter()
                .filter_map(|pinyin| pinyin.first_letter().chars().next())
                .map(|letter| letter.to_ascii_lowercase())
                .collect();
            options.sort_unstable();
            options.dedup();
            if !options.is_empty() {
                initials.push(options);
            }
        } else if ch.is_ascii_alphanumeric() {
            initials.push(vec![ch.to_ascii_lowercase()]);
        }
    }
    initials
}

fn pinyin_matches(initials: &[Vec<char>], query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let mut search_from = 0;
    for segment in alnum_segments(query) {
        let Some(start) = find_segment(initials, &segment, search_from) else {
            return false;
        };
        search_from = start + segment.len();
    }
    true
}

fn alnum_segments(query: &str) -> Vec<Vec<char>> {
    let mut segments: Vec<Vec<char>> = Vec::new();
    let mut previous_digit = None;
    for ch in query.chars() {
        let digit = ch.is_ascii_digit();
        if previous_digit != Some(digit) {
            segments.push(Vec::new());
            previous_digit = Some(digit);
        }
        segments.last_mut().unwrap().push(ch);
    }
    segments
}

fn find_segment(initials: &[Vec<char>], segment: &[char], from: usize) -> Option<usize> {
    let last = initials.len().checked_sub(segment.len())?;
    if from > last {
        return None;
    }
    (from..=last).find(|&start| {
        initials[start..start + segment.len()]
            .iter()
            .zip(segment)
            .all(|(options, ch)| options.contains(ch))
    })
}

fn is_pinyin_query(needle: &str) -> bool {
    needle.chars().all(|ch| ch.is_ascii_alphanumeric())
        && needle.chars().any(|ch| ch.is_ascii_alphabetic())
}

fn compute_filtered(
    entries: &[Entry],
    filter: &str,
    show_all: bool,
    direct_file: Option<&Path>,
) -> Vec<usize> {
    if let Some(direct_file) = direct_file {
        return entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| paths_equal(&entry.path, direct_file))
            .map(|(index, _)| index)
            .collect();
    }

    let needle = filter.to_lowercase();
    let pinyin_query = is_pinyin_query(&needle).then_some(needle.as_str());
    let mut literal = Vec::new();
    let mut phonetic = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        if entry.is_parent {
            if needle.is_empty() {
                literal.push(index);
            }
            continue;
        }
        if !show_all && !entry.is_dir && (!is_docx(&entry.name) || entry.name.starts_with("~$")) {
            continue;
        }
        if needle.is_empty() || entry.name.to_lowercase().contains(&needle) {
            literal.push(index);
        } else if pinyin_query.is_some_and(|query| pinyin_matches(&entry.initials, query)) {
            phonetic.push(index);
        }
    }

    literal.extend(phonetic);
    literal
}

fn is_docx(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("docx"))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn format_modified(time: Option<SystemTime>) -> String {
    time.map(|time| {
        let time: DateTime<Local> = time.into();
        time.format("%Y-%m-%d %H:%M").to_string()
    })
    .unwrap_or_else(|| "-".to_string())
}

fn parse_query(query: &str, cwd: &Path) -> ParsedQuery {
    if query.is_empty() {
        return ParsedQuery {
            dir: cwd.to_path_buf(),
            filter: String::new(),
            mode: Mode::Filter,
        };
    }

    if query == "~"
        && let Some(home) = home_dir()
    {
        return ParsedQuery {
            dir: home,
            filter: String::new(),
            mode: Mode::Path,
        };
    }

    let expanded = expand_home(query);
    let text = normalize_drive(expanded.as_deref().unwrap_or(query));
    if !is_path_like(&text) {
        return ParsedQuery {
            dir: cwd.to_path_buf(),
            filter: query.to_string(),
            mode: Mode::Filter,
        };
    }

    let (dir_text, filter) = split_leaf(&text);
    let dir = resolve_dir_text(if dir_text.is_empty() { "." } else { dir_text }, cwd);
    ParsedQuery {
        dir,
        filter: filter.to_string(),
        mode: Mode::Path,
    }
}

fn resolve_query(query: &str, cwd: &Path) -> ResolvedQuery {
    let parsed = parse_query(query, cwd);
    let mut resolved = ResolvedQuery {
        dir: parsed.dir,
        filter: parsed.filter,
        mode: parsed.mode,
        direct_file: None,
        focus_dir: None,
        error: None,
    };

    match fs::metadata(&resolved.dir) {
        Ok(metadata) if metadata.is_dir() => {
            if !resolved.filter.is_empty() {
                let candidate = resolved.dir.join(&resolved.filter);
                if let Ok(metadata) = fs::metadata(&candidate) {
                    if metadata.is_file() {
                        resolved.direct_file = Some(candidate);
                    } else if metadata.is_dir() {
                        resolved.focus_dir = Some(candidate);
                    }
                }
            }
        }
        Ok(_) => {
            resolved.error = Some(format!("不是目录：{}", resolved.dir.display()));
        }
        Err(err) => {
            resolved.error = Some(format!("路径不存在：{}（{err}）", resolved.dir.display()));
        }
    }

    resolved
}

fn expand_home(query: &str) -> Option<String> {
    #[cfg(windows)]
    let rest = query
        .strip_prefix("~/")
        .or_else(|| query.strip_prefix("~\\"))?;
    #[cfg(not(windows))]
    let rest = query.strip_prefix("~/")?;
    let home = home_dir()?;
    Some(home.join(rest).to_string_lossy().into_owned())
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn normalize_drive(query: &str) -> String {
    #[cfg(windows)]
    {
        if has_drive_prefix(query) {
            let rest = &query[2..];
            if rest.is_empty() {
                return format!("{}\\", &query[..2]);
            }
            if !rest.starts_with('/') && !rest.starts_with('\\') {
                return format!("{}\\{}", &query[..2], rest);
            }
        }
    }
    query.to_string()
}

fn has_drive_prefix(query: &str) -> bool {
    #[cfg(windows)]
    {
        let mut chars = query.chars();
        matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic()) && chars.next() == Some(':')
    }
    #[cfg(not(windows))]
    {
        let _ = query;
        false
    }
}

fn is_path_like(text: &str) -> bool {
    #[cfg(windows)]
    {
        text.contains('/') || text.contains('\\') || has_drive_prefix(text)
    }
    #[cfg(not(windows))]
    {
        text.contains('/')
    }
}

fn split_leaf(text: &str) -> (&str, &str) {
    let mut last = None;
    for (index, ch) in text.char_indices() {
        if is_separator(ch) {
            last = Some(index);
        }
    }
    match last {
        Some(index) => (&text[..=index], &text[index + 1..]),
        None => ("", text),
    }
}

fn is_separator(ch: char) -> bool {
    ch == '/' || (cfg!(windows) && ch == '\\')
}

fn resolve_dir_text(text: &str, cwd: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let rooted = text.starts_with('/') || text.starts_with('\\');
        let unc = text.starts_with("//") || text.starts_with("\\\\");
        if rooted
            && !unc
            && !has_drive_prefix(text)
            && let Some(drive) = current_drive(cwd)
        {
            let mut path = PathBuf::from(format!("{drive}\\"));
            for part in text.split(['/', '\\']) {
                if !part.is_empty() {
                    path.push(part);
                }
            }
            return path;
        }
    }

    let path = Path::new(text);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(windows)]
fn current_drive(cwd: &Path) -> Option<String> {
    use std::path::Component;
    cwd.components().find_map(|component| match component {
        Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().into_owned()),
        _ => None,
    })
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1)
}

fn elide_start(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let total: usize = text.chars().map(char_width).sum();
    if total <= max_width {
        return text.to_string();
    }

    let mut tail = Vec::new();
    let mut used = 1;
    for ch in text.chars().rev() {
        let width = char_width(ch);
        if used + width > max_width {
            break;
        }
        used += width;
        tail.push(ch);
    }
    tail.reverse();
    let mut result = String::from("…");
    result.extend(tail);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("picker-tests")
            .join(format!("{tag}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            path: PathBuf::from(name),
            name: name.to_string(),
            is_dir,
            is_parent: false,
            modified: None,
            initials: initials_of(name),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn plain_query_filters_the_current_directory() {
        let cwd = Path::new("D:/work");
        let parsed = parse_query("总结", cwd);
        assert_eq!(parsed.mode, Mode::Filter);
        assert_eq!(parsed.dir, cwd);
        assert_eq!(parsed.filter, "总结");
    }

    #[test]
    fn empty_query_lists_the_current_directory() {
        let cwd = Path::new("D:/work");
        let parsed = parse_query("", cwd);
        assert_eq!(parsed.mode, Mode::Filter);
        assert_eq!(parsed.dir, cwd);
        assert!(parsed.filter.is_empty());
    }

    #[test]
    fn path_query_splits_directory_and_leaf() {
        let cwd = Path::new("D:/work");
        let query = format!("sub{sep}report.docx", sep = std::path::MAIN_SEPARATOR);
        let parsed = parse_query(&query, cwd);
        assert_eq!(parsed.mode, Mode::Path);
        assert_eq!(parsed.dir, cwd.join("sub"));
        assert_eq!(parsed.filter, "report.docx");
    }

    #[test]
    fn trailing_separator_lists_the_directory_contents() {
        let cwd = Path::new("D:/work");
        let query = format!("sub{}", std::path::MAIN_SEPARATOR);
        let parsed = parse_query(&query, cwd);
        assert_eq!(parsed.mode, Mode::Path);
        assert_eq!(parsed.dir, cwd.join("sub"));
        assert!(parsed.filter.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn leading_slash_is_absolute_on_unix() {
        let cwd = Path::new("/home/rui");
        let parsed = parse_query("/tmp", cwd);
        assert_eq!(parsed.mode, Mode::Path);
        assert_eq!(parsed.dir, Path::new("/"));
        assert_eq!(parsed.filter, "tmp");

        let parsed = parse_query("/tmp/", cwd);
        assert_eq!(parsed.dir, Path::new("/tmp"));
        assert!(parsed.filter.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn drive_and_rooted_paths_are_absolute_on_windows() {
        let cwd = Path::new("D:/work");
        let parsed = parse_query(r"C:\Docs", cwd);
        assert_eq!(parsed.mode, Mode::Path);
        assert_eq!(parsed.dir, PathBuf::from(r"C:\"));
        assert_eq!(parsed.filter, "Docs");

        let parsed = parse_query(r"C:\Docs\", cwd);
        assert_eq!(parsed.dir, PathBuf::from(r"C:\Docs\"));
        assert!(parsed.filter.is_empty());

        let parsed = parse_query(r"\Docs", cwd);
        assert_eq!(parsed.dir, PathBuf::from(r"D:\"));
        assert_eq!(parsed.filter, "Docs");
    }

    #[cfg(windows)]
    #[test]
    fn drive_only_query_resolves_to_the_drive_root() {
        let cwd = Path::new("D:/work");
        let parsed = parse_query("C:", cwd);
        assert_eq!(parsed.mode, Mode::Path);
        assert_eq!(parsed.dir, PathBuf::from(r"C:\"));
        assert!(parsed.filter.is_empty());
    }

    #[test]
    fn docx_filter_keeps_directories_and_hides_other_files() {
        let entries = vec![
            entry("sub", true),
            entry("a.docx", false),
            entry("b.txt", false),
            entry("~$a.docx", false),
        ];
        let filtered = compute_filtered(&entries, "", false, None);
        assert_eq!(filtered, vec![0, 1]);
    }

    #[test]
    fn show_all_reveals_other_files() {
        let entries = vec![entry("a.docx", false), entry("b.txt", false)];
        let filtered = compute_filtered(&entries, "", true, None);
        assert_eq!(filtered, vec![0, 1]);
    }

    #[test]
    fn filter_is_a_case_insensitive_substring() {
        let entries = vec![
            entry("Report-DOCX.docx", false),
            entry("report-old.docx", false),
        ];
        let filtered = compute_filtered(&entries, "REPORT", false, None);
        assert_eq!(filtered, vec![0, 1]);
    }

    #[test]
    fn filter_hides_the_parent_entry_and_non_matching_directories() {
        let entries = vec![
            Entry {
                path: PathBuf::from(".."),
                name: "..".to_string(),
                is_dir: true,
                is_parent: true,
                modified: None,
                initials: Vec::new(),
            },
            entry("sub", true),
            entry("other", true),
            entry("2023-old.docx", false),
        ];
        let filtered = compute_filtered(&entries, "old", false, None);
        assert_eq!(filtered, vec![3]);
    }

    #[test]
    fn pinyin_initials_match_chinese_names() {
        let initials = initials_of("工作总结");
        assert_eq!(initials, vec![vec!['g'], vec!['z'], vec!['z'], vec!['j']]);
        assert!(pinyin_matches(&initials, "gzzj"));
        assert!(!pinyin_matches(&initials, "gzj"));
    }

    #[test]
    fn pinyin_matches_digits_and_multiple_segments() {
        let initials = initials_of("2023年工作总结");
        assert!(pinyin_matches(&initials, "gzzj"));
        assert!(pinyin_matches(&initials, "2023gzzj"));
        assert!(pinyin_matches(&initials, "2023"));
    }

    #[test]
    fn pinyin_accepts_either_reading_of_polyphonic_characters() {
        let chongqing = initials_of("重庆");
        assert!(pinyin_matches(&chongqing, "cq"));
        assert!(pinyin_matches(&chongqing, "zq"));

        let bank = initials_of("银行");
        assert!(pinyin_matches(&bank, "yh"));
        assert!(pinyin_matches(&bank, "yx"));
    }

    #[test]
    fn pinyin_matching_prefers_literal_hits() {
        let entries = vec![entry("工作总结.docx", false), entry("zz-gz.docx", false)];
        let filtered = compute_filtered(&entries, "gz", false, None);
        assert_eq!(filtered, vec![1, 0]);
    }

    #[test]
    fn pinyin_matching_is_skipped_for_mixed_queries() {
        let entries = vec![entry("工作总结.docx", false)];
        assert!(compute_filtered(&entries, "gz zj", false, None).is_empty());
        assert_eq!(compute_filtered(&entries, "总结", false, None), vec![0]);
        assert_eq!(compute_filtered(&entries, "GZ", false, None), vec![0]);
    }

    #[test]
    fn pinyin_matching_applies_to_directories() {
        let entries = vec![entry("安全监察部", true)];
        assert_eq!(compute_filtered(&entries, "aqjcb", false, None), vec![0]);
    }

    #[test]
    fn direct_file_wins_over_the_docx_filter() {
        let base = std::path::Path::new("D:/work");
        let entries = vec![entry("a.docx", false), entry("b.txt", false)];
        let mut entries = entries;
        entries[1].path = base.join("b.txt");
        let filtered = compute_filtered(&entries, "b.txt", false, Some(&base.join("b.txt")));
        assert_eq!(filtered, vec![1]);
    }

    #[test]
    fn picker_navigates_and_picks_a_docx() {
        let dir = temp_dir("pick");
        fs::write(dir.join("report.docx"), b"x").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();

        let mut picker = Picker::new(Role::Old, dir.clone(), None);
        assert!(
            picker
                .filtered
                .iter()
                .any(|&index| { picker.entries[index].name == "report.docx" })
        );
        assert!(
            !picker
                .filtered
                .iter()
                .any(|&index| { picker.entries[index].name == "notes.txt" })
        );

        picker.set_query("sub");
        assert_eq!(picker.current_entry().unwrap().name, "sub");
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            Action::Stay
        ));
        assert_eq!(picker.cwd, dir.join("sub"));

        picker.handle_key(key(KeyCode::Backspace));
        assert_eq!(picker.cwd, dir);

        picker.set_query("report.docx");
        match picker.handle_key(key(KeyCode::Enter)) {
            Action::Pick(path) => assert_eq!(path, dir.join("report.docx")),
            _ => panic!("应当选中 report.docx"),
        }
    }

    #[test]
    fn picker_blocks_non_docx_files() {
        let dir = temp_dir("block");
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        let mut picker = Picker::new(Role::Old, dir, None);

        assert!(!picker.show_all);
        picker.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        picker.set_query("notes.txt");

        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            Action::Stay
        ));
        assert!(
            picker
                .status
                .as_deref()
                .is_some_and(|status| status.contains("只支持 .docx"))
        );
    }

    #[test]
    fn picker_starts_in_the_selected_file_directory() {
        let dir = temp_dir("start");
        fs::create_dir(dir.join("inner")).unwrap();
        let start = start_dir_for(Some(&dir.join("inner").join("old.docx")));
        assert_eq!(start, dir.join("inner"));
    }

    #[test]
    fn long_paths_keep_their_tail() {
        let elided = elide_start("D:/very/long/path/report.docx", 10);
        assert!(elided.starts_with('…'));
        assert!(elided.ends_with("t.docx"));
    }
}
