use std::io;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::engine::DocumentKind;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const OPTIONS: [(DocumentKind, &str, &str); 2] = [
    (
        DocumentKind::Docx,
        "Word 文档",
        "比较段落、表格行与单元格差异（.docx）",
    ),
    (
        DocumentKind::Xlsx,
        "Excel 工作簿",
        "比较工作表、行、列与单元格（.xlsx / .xlsm，首版）",
    ),
];

pub fn choose_kind(terminal: &mut TuiTerminal) -> Result<Option<DocumentKind>> {
    let mut selected = 0usize;
    loop {
        terminal.draw(|frame| draw(frame, selected))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(OPTIONS.len() - 1);
            }
            KeyCode::Char('1') => selected = 0,
            KeyCode::Char('2') => selected = 1,
            KeyCode::Enter => return Ok(Some(OPTIONS[selected].0)),
            _ => {}
        }
    }
}

fn draw(frame: &mut ratatui::Frame, selected: usize) {
    let area = centered(frame.area(), 78, 18);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" diffRact · 选择比较类型 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);

    frame.render_widget(
        Paragraph::new("选择要比较的文件类型，随后进入文件选择器。")
            .style(Style::default().fg(Color::Gray)),
        chunks[0],
    );

    for (index, (_, name, description)) in OPTIONS.iter().enumerate() {
        let chosen = index == selected;
        let style = if chosen {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let lines = vec![
            Line::from(vec![
                Span::styled(format!(" {} ", index + 1), style),
                Span::styled(format!(" {name}"), style),
            ]),
            Line::from(Span::styled(
                format!("   {description}"),
                if chosen {
                    style
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).border_style(
                if chosen {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            )),
            chunks[index + 1],
        );
    }

    frame.render_widget(
        Paragraph::new("Excel 首版比较值或公式，不比较样式、图表或批注。")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new("↑↓ / j k 选择   Enter 确认   1/2 直达   Esc 退出")
            .alignment(Alignment::Left)
            .style(Style::default().fg(Color::DarkGray)),
        chunks[4],
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}
