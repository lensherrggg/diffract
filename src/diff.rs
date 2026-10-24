use similar::{ChangeTag, TextDiff};

use crate::result::{DiffOp, DiffSpan, ParaDiff};

pub fn para_diff(old: &str, new: &str) -> ParaDiff {
    let diff = TextDiff::from_chars(old, new);
    let mut old_spans: Vec<DiffSpan> = Vec::new();
    let mut new_spans: Vec<DiffSpan> = Vec::new();

    for change in diff.iter_all_changes() {
        let value = change.value();
        if value.is_empty() {
            continue;
        }
        match change.tag() {
            ChangeTag::Equal => {
                push(&mut old_spans, DiffOp::Equal, value);
                push(&mut new_spans, DiffOp::Equal, value);
            }
            ChangeTag::Delete => push(&mut old_spans, DiffOp::Delete, value),
            ChangeTag::Insert => push(&mut new_spans, DiffOp::Insert, value),
        }
    }

    ParaDiff {
        old: old_spans,
        new: new_spans,
    }
}

fn push(spans: &mut Vec<DiffSpan>, op: DiffOp, value: &str) {
    if let Some(last) = spans.last_mut() {
        if last.op == op {
            last.text.push_str(value);
            return;
        }
    }
    spans.push(DiffSpan {
        op,
        text: value.to_string(),
    });
}

/// 只保留第一处到最后一起变更前后 `context` 个字符；被裁掉的一侧用省略号提示。
pub fn excerpt(spans: &[DiffSpan], context: usize) -> Vec<DiffSpan> {
    if spans.is_empty() {
        return Vec::new();
    }
    let lengths: Vec<usize> = spans.iter().map(|span| span.text.chars().count()).collect();
    let total: usize = lengths.iter().sum();
    let first_changed = spans.iter().position(|s| s.op != DiffOp::Equal);
    let last_changed = spans.iter().rposition(|s| s.op != DiffOp::Equal);

    let (Some(first), Some(last)) = (first_changed, last_changed) else {
        return slice_chars(spans, &lengths, 0, total.min(context.saturating_mul(2)));
    };

    let first_start: usize = lengths[..first].iter().sum();
    let last_end: usize = lengths[..=last].iter().sum();
    let start = first_start.saturating_sub(context);
    let end = last_end.saturating_add(context).min(total);

    let mut result = Vec::new();
    if start > 0 {
        result.push(DiffSpan {
            op: DiffOp::Equal,
            text: "…".to_string(),
        });
    }
    result.extend(slice_chars(spans, &lengths, start, end));
    if end < total {
        result.push(DiffSpan {
            op: DiffOp::Equal,
            text: "…".to_string(),
        });
    }
    result
}

fn slice_chars(spans: &[DiffSpan], lengths: &[usize], start: usize, end: usize) -> Vec<DiffSpan> {
    let mut result = Vec::new();
    let mut offset = 0usize;
    for (span, len) in spans.iter().zip(lengths.iter()) {
        let span_start = offset;
        let span_end = offset + len;
        offset = span_end;
        if span_end <= start || span_start >= end {
            continue;
        }
        let from = start.saturating_sub(span_start).min(*len);
        let to = end.saturating_sub(span_start).min(*len);
        if from >= to {
            continue;
        }
        let text: String = span.text.chars().skip(from).take(to - from).collect();
        push(&mut result, span.op, &text);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_both_sides() {
        let diff = para_diff("公司应当每年检查一次", "公司应当每季度检查一次");
        let old_text: String = diff.old.iter().map(|s| s.text.as_str()).collect();
        let new_text: String = diff.new.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(old_text, "公司应当每年检查一次");
        assert_eq!(new_text, "公司应当每季度检查一次");
        assert!(diff.old.iter().any(|s| s.op == DiffOp::Delete));
        assert!(diff.new.iter().any(|s| s.op == DiffOp::Insert));
    }

    #[test]
    fn excerpt_keeps_context_around_change() {
        let old = format!("{}旧{}", "甲".repeat(100), "乙".repeat(100));
        let new = format!("{}新{}", "甲".repeat(100), "乙".repeat(100));
        let diff = para_diff(&old, &new);
        let clipped = excerpt(&diff.old, 5);
        let text: String = clipped.iter().map(|s| s.text.as_str()).collect();
        assert!(text.starts_with('…'));
        assert!(text.ends_with('…'));
        assert!(text.contains('旧'));
        assert!(text.chars().count() < 20);
    }
}
