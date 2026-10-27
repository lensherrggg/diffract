use crate::result::{DiffOp, ParaDiff, Record, Status, TableLocation};

/// 修订视图中的块类型。正文以新文段落为骨架，删除与移动原位标记穿插其中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionKind {
    Unchanged,
    Added,
    Removed,
    Modified,
    SuspectedModified,
    MovedFrom,
    MovedTo,
    Table,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSpan {
    pub op: DiffOp,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSummary {
    pub table: usize,
    pub caption: Option<String>,
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
    pub moved: usize,
    /// 整表未配对时记录的新增/删除状态；此时行计数没有意义。
    pub whole: Option<Status>,
}

#[derive(Debug, Clone)]
pub struct RevisionBlock {
    pub kind: RevisionKind,
    pub order: f64,
    pub old_index: Option<usize>,
    pub new_index: Option<usize>,
    pub note: Option<String>,
    pub spans: Vec<RevisionSpan>,
    pub table: Option<TableSummary>,
}

const POSITION_EPSILON: f64 = 1e-6;

/// 把记录流整理成修订视图块。记录的 order 已经按新文阅读顺序排好，删除段在旧位置附近。
pub fn build(records: &[Record]) -> Vec<RevisionBlock> {
    let mut blocks = Vec::new();
    let mut moved_from = Vec::new();
    let mut tables: Vec<(f64, TableSummary)> = Vec::new();

    for record in records {
        if let Some(location) = record.table.as_ref() {
            push_table(&mut tables, location, record);
            continue;
        }
        blocks.push(paragraph_block(record, &mut moved_from));
    }

    blocks.extend(tables.into_iter().map(|(order, table)| RevisionBlock {
        kind: RevisionKind::Table,
        order,
        old_index: None,
        new_index: None,
        note: None,
        spans: Vec::new(),
        table: Some(table),
    }));
    place_moved_from(&mut blocks, moved_from);
    blocks.sort_by(|a, b| {
        a.order
            .total_cmp(&b.order)
            .then_with(|| a.old_index.cmp(&b.old_index))
            .then_with(|| a.new_index.cmp(&b.new_index))
            .then_with(|| kind_rank(a.kind).cmp(&kind_rank(b.kind)))
    });
    blocks
}

fn paragraph_block(record: &Record, moved_from: &mut Vec<RevisionBlock>) -> RevisionBlock {
    let (kind, spans) = match record.status {
        Status::Unchanged => (
            RevisionKind::Unchanged,
            spans_for(
                record.new_text.as_deref().or(record.old_text.as_deref()),
                DiffOp::Equal,
            ),
        ),
        Status::Added => (
            RevisionKind::Added,
            spans_for(record.new_text.as_deref(), DiffOp::Insert),
        ),
        Status::Removed => (
            RevisionKind::Removed,
            spans_for(record.old_text.as_deref(), DiffOp::Delete),
        ),
        Status::Modified => (RevisionKind::Modified, merged_spans(record)),
        Status::SuspectedModified => (RevisionKind::SuspectedModified, merged_spans(record)),
        Status::Moved => {
            if let (Some(old), Some(new)) = (record.old_para_index, record.new_para_index) {
                moved_from.push(RevisionBlock {
                    kind: RevisionKind::MovedFrom,
                    order: 0.0,
                    old_index: Some(old),
                    new_index: None,
                    note: Some(format!("移动至新第 {new} 段")),
                    spans: spans_for(record.old_text.as_deref(), DiffOp::Delete),
                    table: None,
                });
            }
            (
                RevisionKind::MovedTo,
                spans_for(record.new_text.as_deref(), DiffOp::Insert),
            )
        }
    };
    let note = match kind {
        RevisionKind::MovedTo => record
            .old_para_index
            .map(|old| format!("移动自旧第 {old} 段")),
        RevisionKind::Modified | RevisionKind::SuspectedModified => record.note.clone(),
        _ => None,
    };
    RevisionBlock {
        kind,
        order: record.order,
        old_index: record.old_para_index,
        new_index: record.new_para_index,
        note,
        spans,
        table: None,
    }
}

/// 修改段把旧侧删除片段和新侧插入片段合并成一条阅读顺序。
pub fn merged_spans(record: &Record) -> Vec<RevisionSpan> {
    let Some(diff) = record.diff.as_ref() else {
        let mut spans = Vec::new();
        if let Some(old) = record.old_text.as_deref() {
            push_span(&mut spans, DiffOp::Delete, old);
        }
        if let Some(new) = record.new_text.as_deref() {
            push_span(&mut spans, DiffOp::Insert, new);
        }
        return spans;
    };
    merge_diff(diff)
}

pub fn merge_diff(diff: &ParaDiff) -> Vec<RevisionSpan> {
    let mut spans = Vec::new();
    let (mut old_at, mut new_at) = (0usize, 0usize);
    while old_at < diff.old.len() || new_at < diff.new.len() {
        match (diff.old.get(old_at), diff.new.get(new_at)) {
            (Some(old), Some(new)) if old.op == DiffOp::Equal && new.op == DiffOp::Equal => {
                // 两侧相等片段由同一个 TextDiff 生成，取新文一侧即可。
                push_span(&mut spans, DiffOp::Equal, &new.text);
                old_at += 1;
                new_at += 1;
            }
            (Some(old), _) if old.op == DiffOp::Delete => {
                push_span(&mut spans, DiffOp::Delete, &old.text);
                old_at += 1;
            }
            (_, Some(new)) if new.op == DiffOp::Insert => {
                push_span(&mut spans, DiffOp::Insert, &new.text);
                new_at += 1;
            }
            (Some(old), _) => {
                push_span(&mut spans, old.op, &old.text);
                old_at += 1;
            }
            (_, Some(new)) => {
                push_span(&mut spans, new.op, &new.text);
                new_at += 1;
            }
            (None, None) => break,
        }
    }
    spans
}

fn spans_for(text: Option<&str>, op: DiffOp) -> Vec<RevisionSpan> {
    let mut spans = Vec::new();
    if let Some(text) = text {
        push_span(&mut spans, op, text);
    }
    spans
}

fn push_span(spans: &mut Vec<RevisionSpan>, op: DiffOp, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.op == op
    {
        last.text.push_str(text);
        return;
    }
    spans.push(RevisionSpan {
        op,
        text: text.to_string(),
    });
}

fn push_table(tables: &mut Vec<(f64, TableSummary)>, location: &TableLocation, record: &Record) {
    let index = match tables
        .iter()
        .position(|(_, summary)| summary.table == location.table)
    {
        Some(index) => index,
        None => {
            tables.push((
                record.order,
                TableSummary {
                    table: location.table,
                    caption: location.caption.clone(),
                    added: 0,
                    removed: 0,
                    modified: 0,
                    moved: 0,
                    whole: None,
                },
            ));
            tables.len() - 1
        }
    };
    let (order, summary) = &mut tables[index];
    *order = (*order).min(record.order);
    if summary.caption.is_none() {
        summary.caption = location.caption.clone();
    }
    if location.old_row.is_none()
        && location.new_row.is_none()
        && matches!(record.status, Status::Added | Status::Removed)
    {
        summary.whole = Some(record.status);
    }
    match record.status {
        Status::Added => summary.added += 1,
        Status::Removed => summary.removed += 1,
        Status::Modified | Status::SuspectedModified => summary.modified += 1,
        Status::Moved => summary.moved += 1,
        Status::Unchanged => {}
    }
}

/// 移动段在原位置补一个删除块：放在所有更早旧段落之后，没有更早段落则放在更晚段落之前。
/// 同一旧位置附近的移动块由排序时的旧段号保持先后。
fn place_moved_from(blocks: &mut Vec<RevisionBlock>, mut moved: Vec<RevisionBlock>) {
    moved.sort_by_key(|block| (block.old_index, block.new_index));
    // 移动段的新位置块不再代表旧位置，不能拿它给其它移动段定位。
    let mut known: Vec<(usize, f64)> = blocks
        .iter()
        .filter(|block| block.kind != RevisionKind::MovedTo)
        .filter_map(|block| block.old_index.map(|old| (old, block.order)))
        .collect();

    for mut block in moved {
        let Some(old) = block.old_index else {
            continue;
        };
        let older = known
            .iter()
            .filter(|(index, _)| *index < old)
            .map(|(_, order)| *order)
            .reduce(f64::max);
        let younger = known
            .iter()
            .filter(|(index, _)| *index > old)
            .map(|(_, order)| *order)
            .reduce(f64::min);
        block.order = if let Some(order) = older {
            order + POSITION_EPSILON
        } else if let Some(order) = younger {
            order - POSITION_EPSILON
        } else {
            old as f64
        };
        known.push((old, block.order));
        blocks.push(block);
    }
}

fn kind_rank(kind: RevisionKind) -> u8 {
    match kind {
        RevisionKind::Removed | RevisionKind::MovedFrom => 0,
        RevisionKind::Table => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::para_diff;
    use crate::result::Scores;

    fn paragraph(
        status: Status,
        old: Option<usize>,
        new: Option<usize>,
        old_text: Option<&str>,
        new_text: Option<&str>,
        order: f64,
    ) -> Record {
        Record {
            id: 0,
            status,
            old_para_index: old,
            new_para_index: new,
            old_text: old_text.map(str::to_string),
            new_text: new_text.map(str::to_string),
            scores: Scores::empty(),
            diff: (status.is_modified_like())
                .then(|| para_diff(old_text.unwrap_or(""), new_text.unwrap_or(""))),
            row_diff: None,
            matched_to: new,
            note: None,
            table: None,
            order,
        }
    }

    fn kinds(blocks: &[RevisionBlock]) -> Vec<RevisionKind> {
        blocks.iter().map(|block| block.kind).collect()
    }

    #[test]
    fn moved_paragraph_gets_deleted_at_original_position() {
        let records = vec![
            paragraph(
                Status::Unchanged,
                Some(1),
                Some(1),
                Some("甲"),
                Some("甲"),
                0.0,
            ),
            paragraph(Status::Moved, Some(2), Some(3), Some("乙"), Some("乙"), 2.0),
            paragraph(
                Status::Unchanged,
                Some(3),
                Some(2),
                Some("丙"),
                Some("丙"),
                1.0,
            ),
        ];

        let blocks = build(&records);
        assert_eq!(
            kinds(&blocks),
            vec![
                RevisionKind::Unchanged,
                RevisionKind::MovedFrom,
                RevisionKind::Unchanged,
                RevisionKind::MovedTo,
            ]
        );
        assert_eq!(blocks[1].old_index, Some(2));
        assert_eq!(blocks[1].note.as_deref(), Some("移动至新第 3 段"));
        assert_eq!(blocks[3].new_index, Some(3));
        assert_eq!(blocks[3].note.as_deref(), Some("移动自旧第 2 段"));
        assert_eq!(blocks[1].spans[0].op, DiffOp::Delete);
        assert_eq!(blocks[3].spans[0].op, DiffOp::Insert);
    }

    #[test]
    fn modified_spans_mix_old_deletions_and_new_insertions() {
        let old = "公司应当每年检查一次";
        let new = "公司应当每季度检查一次";
        let records = vec![paragraph(
            Status::Modified,
            Some(2),
            Some(2),
            Some(old),
            Some(new),
            1.0,
        )];

        let blocks = build(&records);
        assert_eq!(blocks.len(), 1);
        let spans = &blocks[0].spans;
        assert!(spans.iter().any(|span| span.op == DiffOp::Delete));
        assert!(spans.iter().any(|span| span.op == DiffOp::Insert));
        let deleted: String = spans
            .iter()
            .filter(|span| span.op == DiffOp::Delete)
            .map(|span| span.text.as_str())
            .collect();
        let new_side: String = spans
            .iter()
            .filter(|span| span.op != DiffOp::Delete)
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!(deleted, "年");
        assert_eq!(new_side, new);
    }

    #[test]
    fn modified_without_diff_falls_back_to_delete_then_insert() {
        let mut record = paragraph(
            Status::Modified,
            Some(1),
            Some(1),
            Some("旧文本"),
            Some("新文本"),
            0.0,
        );
        record.diff = None;

        let blocks = build(&[record]);
        assert_eq!(
            blocks[0].spans,
            vec![
                RevisionSpan {
                    op: DiffOp::Delete,
                    text: "旧文本".to_string(),
                },
                RevisionSpan {
                    op: DiffOp::Insert,
                    text: "新文本".to_string(),
                },
            ]
        );
    }

    #[test]
    fn moved_from_keeps_relative_old_order() {
        let records = vec![
            paragraph(Status::Moved, Some(1), Some(4), Some("甲"), Some("甲"), 3.0),
            paragraph(Status::Removed, Some(2), None, Some("乙"), None, 0.5),
            paragraph(
                Status::Unchanged,
                Some(3),
                Some(2),
                Some("丙"),
                Some("丙"),
                1.0,
            ),
            paragraph(
                Status::Unchanged,
                Some(4),
                Some(1),
                Some("丁"),
                Some("丁"),
                0.0,
            ),
            paragraph(
                Status::Unchanged,
                Some(5),
                Some(3),
                Some("戊"),
                Some("戊"),
                2.0,
            ),
        ];

        let blocks = build(&records);
        let order: Vec<String> = blocks
            .iter()
            .map(|block| {
                format!(
                    "{:?}:{}",
                    block.kind,
                    block
                        .spans
                        .first()
                        .map(|span| span.text.as_str())
                        .unwrap_or("")
                )
            })
            .collect();
        let moved_from = blocks
            .iter()
            .position(|block| block.kind == RevisionKind::MovedFrom)
            .unwrap();
        assert!(
            moved_from
                < blocks
                    .iter()
                    .position(|block| block.spans.first().is_some_and(|span| span.text == "丙"))
                    .unwrap(),
            "{order:?}"
        );
    }

    #[test]
    fn whole_table_record_is_marked_as_whole() {
        let mut record = paragraph(Status::Removed, None, None, Some("甲 | 1"), None, 0.0);
        record.table = Some(TableLocation {
            table: 2,
            old_row: None,
            new_row: None,
            caption: None,
        });

        let blocks = build(&[record]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].table.as_ref().unwrap().whole,
            Some(Status::Removed)
        );
    }

    #[test]
    fn chained_moves_keep_old_order_at_original_positions() {
        let records = vec![
            paragraph(
                Status::Unchanged,
                Some(1),
                Some(1),
                Some("甲"),
                Some("甲"),
                0.0,
            ),
            paragraph(Status::Moved, Some(2), Some(5), Some("乙"), Some("乙"), 4.0),
            paragraph(Status::Moved, Some(3), Some(4), Some("丙"), Some("丙"), 3.0),
            paragraph(
                Status::Unchanged,
                Some(4),
                Some(2),
                Some("丁"),
                Some("丁"),
                1.0,
            ),
            paragraph(
                Status::Unchanged,
                Some(5),
                Some(3),
                Some("戊"),
                Some("戊"),
                2.0,
            ),
        ];

        let blocks = build(&records);
        let first_with_text = |text: &str| {
            blocks
                .iter()
                .position(|block| block.spans.first().is_some_and(|span| span.text == text))
                .unwrap()
        };
        let moved_from = |text: &str| {
            blocks
                .iter()
                .position(|block| {
                    block.kind == RevisionKind::MovedFrom
                        && block.spans.first().is_some_and(|span| span.text == text)
                })
                .unwrap()
        };
        assert!(first_with_text("甲") < moved_from("乙"));
        assert!(moved_from("乙") < moved_from("丙"));
        assert!(moved_from("丙") < first_with_text("丁"));
    }

    #[test]
    fn table_rows_are_grouped_into_one_summary_block() {
        let location = |row: usize| TableLocation {
            table: 1,
            old_row: None,
            new_row: Some(row),
            caption: Some("指标完成情况".to_string()),
        };
        let mut added = paragraph(Status::Added, None, None, None, Some("甲 | 1"), 1.0);
        added.table = Some(location(2));
        let mut removed = paragraph(Status::Removed, None, None, Some("乙 | 2"), None, 1.0);
        removed.table = Some(location(3));
        let mut modified = paragraph(
            Status::Modified,
            None,
            None,
            Some("丙 | 3"),
            Some("丙 | 4"),
            1.0,
        );
        modified.table = Some(location(4));

        let blocks = build(&[added, removed, modified]);
        assert_eq!(blocks.len(), 1);
        let table = blocks[0].table.as_ref().unwrap();
        assert_eq!(table.added, 1);
        assert_eq!(table.removed, 1);
        assert_eq!(table.modified, 1);
        assert_eq!(table.caption.as_deref(), Some("指标完成情况"));
    }
}
