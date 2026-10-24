use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;

use crate::result::{CompareOutput, DiffOp, Record, SemanticStatus, Status, Summary};

pub fn default_export_path(extension: &str) -> PathBuf {
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    PathBuf::from(format!("diffract-report-{stamp}.{extension}"))
}

pub fn check_report(output: &CompareOutput) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "旧文件：{}（{} 段，{} 表）",
        CompareOutput::file_label(&output.old_path),
        output.old_paragraphs,
        output.old_tables
    );
    let _ = writeln!(
        text,
        "新文件：{}（{} 段，{} 表）",
        CompareOutput::file_label(&output.new_path),
        output.new_paragraphs,
        output.new_tables
    );
    let _ = writeln!(text, "语义匹配：{}", semantic_label(&output.semantic));
    let _ = writeln!(text, "结果：{}", output.summary.brief());
    let _ = writeln!(
        text,
        "结论：{}",
        if output.summary.has_differences() {
            "存在差异"
        } else {
            "无差异"
        }
    );
    let _ = writeln!(text, "耗时：{:.2}s", output.elapsed.as_secs_f64());
    text
}

fn semantic_label(status: &SemanticStatus) -> String {
    if status.enabled {
        status.short_label()
    } else {
        format!("未启用（{}）", status.reason.as_deref().unwrap_or("未配置"))
    }
}

pub fn export_html(output: &CompareOutput, path: &Path) -> Result<()> {
    ensure_parent(path)?;
    let html = render_html(output);
    std::fs::write(path, html)
        .with_context(|| format!("写入 HTML 报告失败：{}", path.display()))?;
    log::info!("已导出 HTML 报告：{}", path.display());
    Ok(())
}

pub fn export_markdown(output: &CompareOutput, path: &Path) -> Result<()> {
    ensure_parent(path)?;
    let markdown = render_markdown(output);
    std::fs::write(path, markdown)
        .with_context(|| format!("写入 Markdown 报告失败：{}", path.display()))?;
    log::info!("已导出 Markdown 报告：{}", path.display());
    Ok(())
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建导出目录失败：{}", parent.display()))?;
        }
    }
    Ok(())
}

fn confirmed_records(output: &CompareOutput) -> Vec<&Record> {
    output
        .records
        .iter()
        .filter(|record| {
            matches!(
                record.status,
                Status::Added | Status::Removed | Status::Modified | Status::Moved
            )
        })
        .collect()
}

fn suspected_records(output: &CompareOutput) -> Vec<&Record> {
    output
        .records
        .iter()
        .filter(|record| record.status == Status::SuspectedModified)
        .collect()
}

fn render_html(output: &CompareOutput) -> String {
    let mut html = String::new();
    let title = format!(
        "diffRact 比较报告：{} ↔ {}",
        CompareOutput::file_label(&output.old_path),
        CompareOutput::file_label(&output.new_path)
    );
    html.push_str("<!DOCTYPE html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(html, "<title>{}</title>", escape_html(&title));
    html.push_str(
        r#"<style>
:root{color-scheme:light}
body{font-family:"Microsoft YaHei","PingFang SC",system-ui,sans-serif;max-width:1100px;margin:0 auto;padding:24px;line-height:1.7;color:#1f2937;background:#f8fafc}
h1{font-size:24px;margin:0 0 8px}
h2{font-size:19px;margin:32px 0 12px;border-left:4px solid #2563eb;padding-left:10px}
.meta{color:#64748b;font-size:13px}
.notice{margin:16px 0;padding:10px 14px;border-radius:6px;background:#fef3c7;border:1px solid #f59e0b;color:#92400e}
.notice.ok{background:#ecfdf5;border-color:#10b981;color:#065f46}
.summary{display:flex;flex-wrap:wrap;gap:10px;margin:18px 0}
.stat{background:#fff;border:1px solid #e2e8f0;border-radius:8px;padding:10px 16px;min-width:92px}
.stat b{display:block;font-size:22px}
.stat span{font-size:12px;color:#64748b}
.card{background:#fff;border:1px solid #e2e8f0;border-radius:10px;padding:16px 18px;margin:14px 0;box-shadow:0 1px 2px rgba(15,23,42,.04)}
.card h3{font-size:16px;margin:0 0 8px;display:flex;gap:10px;align-items:center;flex-wrap:wrap}
.badge{display:inline-block;border-radius:999px;padding:1px 10px;font-size:12px;font-weight:600;color:#fff}
.badge.modified{background:#d97706}.badge.suspected_modified{background:#9333ea}
.badge.added{background:#059669}.badge.removed{background:#dc2626}.badge.moved{background:#0891b2}
.kv{font-size:12px;color:#475569;margin-bottom:10px;display:flex;gap:16px;flex-wrap:wrap}
.text{white-space:pre-wrap;word-break:break-word;background:#f8fafc;border-radius:6px;padding:8px 10px;margin:6px 0}
.label{font-size:12px;color:#64748b;margin-top:10px}
del{background:#fee2e2;color:#991b1b;text-decoration:line-through}
ins{background:#d1fae5;color:#065f46;text-decoration:none}
.empty{color:#94a3b8}
.note{font-size:12px;color:#7c3aed}
</style>
</head>
<body>
"#,
    );
    let _ = writeln!(html, "<h1>{}</h1>", escape_html(&title));
    let _ = writeln!(
        html,
        "<div class=\"meta\">生成时间：{}</div>",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    let _ = writeln!(
        html,
        "<div class=\"meta\">旧文件：{}（{} 段，{} 表）　新文件：{}（{} 段，{} 表）　耗时：{:.2}s</div>",
        escape_html(&CompareOutput::file_label(&output.old_path)),
        output.old_paragraphs,
        output.old_tables,
        escape_html(&CompareOutput::file_label(&output.new_path)),
        output.new_paragraphs,
        output.new_tables,
        output.elapsed.as_secs_f64()
    );
    let notice_class = if output.semantic.enabled {
        "notice ok"
    } else {
        "notice"
    };
    let _ = writeln!(
        html,
        "<div class=\"{}\">语义匹配：{}</div>",
        notice_class,
        escape_html(&semantic_label(&output.semantic))
    );
    render_summary_html(&mut html, &output.summary);

    html.push_str("<h2>确定差异</h2>\n");
    let confirmed = confirmed_records(output);
    if confirmed.is_empty() {
        html.push_str("<p class=\"empty\">无</p>\n");
    } else {
        for record in confirmed {
            render_record_html(&mut html, record);
        }
    }

    html.push_str("<h2>疑似修改待确认</h2>\n");
    let suspected = suspected_records(output);
    if suspected.is_empty() {
        html.push_str("<p class=\"empty\">无</p>\n");
    } else {
        for record in suspected {
            render_record_html(&mut html, record);
        }
    }
    html.push_str("</body>\n</html>\n");
    html
}

fn render_summary_html(html: &mut String, summary: &Summary) {
    html.push_str("<div class=\"summary\">\n");
    for (label, value) in [
        ("未变", summary.unchanged),
        ("修改", summary.modified),
        ("疑似修改", summary.suspected_modified),
        ("新增", summary.added),
        ("删除", summary.removed),
        ("移动", summary.moved),
    ] {
        let _ = writeln!(
            html,
            "<div class=\"stat\"><b>{value}</b><span>{label}</span></div>"
        );
    }
    html.push_str("</div>\n");
}

fn render_record_html(html: &mut String, record: &Record) {
    html.push_str("<div class=\"card\">\n");
    let _ = writeln!(
        html,
        "<h3>#{} <span class=\"badge {}\">{}</span>{} → {}</h3>",
        record.id,
        record.status.as_str(),
        record.status.label_zh(),
        position_label(record, true),
        position_label(record, false)
    );
    let semantic = record
        .scores
        .semantic
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "—".to_string());
    let _ = writeln!(
        html,
        "<div class=\"kv\"><span>文本相似度 {:.4}</span><span>语义相似度 {}</span><span>综合分 {:.4}</span><span>matched_to {}</span></div>",
        record.scores.text,
        semantic,
        record.scores.total,
        record
            .matched_to
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".to_string())
    );
    if let Some(note) = &record.note {
        let _ = writeln!(
            html,
            "<div class=\"note\">备注：{}</div>",
            escape_html(note)
        );
    }

    match record.status {
        Status::Modified | Status::SuspectedModified => {
            if let Some(diff) = &record.diff {
                render_diff_html(html, "旧文（删除线为旧内容）", &diff.old);
                render_diff_html(html, "新文（绿色为新增内容）", &diff.new);
            }
        }
        Status::Added => {
            if let Some(text) = &record.new_text {
                html.push_str("<div class=\"label\">新增内容</div>\n");
                let _ = writeln!(html, "<div class=\"text\">{}</div>", format_text_html(text));
            }
        }
        Status::Removed => {
            if let Some(text) = &record.old_text {
                html.push_str("<div class=\"label\">删除内容</div>\n");
                let _ = writeln!(html, "<div class=\"text\">{}</div>", format_text_html(text));
            }
        }
        Status::Moved => {
            if let Some(text) = &record.new_text {
                html.push_str("<div class=\"label\">移动内容</div>\n");
                let _ = writeln!(html, "<div class=\"text\">{}</div>", format_text_html(text));
            }
        }
        Status::Unchanged => {}
    }
    html.push_str("</div>\n");
}

fn render_diff_html(html: &mut String, label: &str, spans: &[crate::result::DiffSpan]) {
    html.push_str("<div class=\"label\">");
    html.push_str(&escape_html(label));
    html.push_str("</div>\n<div class=\"text\">");
    for span in spans {
        let text = format_text_html(&span.text);
        match span.op {
            DiffOp::Equal => html.push_str(&text),
            DiffOp::Delete => {
                let _ = write!(html, "<del>{text}</del>");
            }
            DiffOp::Insert => {
                let _ = write!(html, "<ins>{text}</ins>");
            }
        }
    }
    html.push_str("</div>\n");
}

fn render_markdown(output: &CompareOutput) -> String {
    let mut md = String::new();
    let _ = writeln!(
        md,
        "# diffRact 比较报告：{} ↔ {}",
        CompareOutput::file_label(&output.old_path),
        CompareOutput::file_label(&output.new_path)
    );
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "- 生成时间：{}",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    let _ = writeln!(
        md,
        "- 旧文件：{}（{} 段，{} 表）",
        CompareOutput::file_label(&output.old_path),
        output.old_paragraphs,
        output.old_tables
    );
    let _ = writeln!(
        md,
        "- 新文件：{}（{} 段，{} 表）",
        CompareOutput::file_label(&output.new_path),
        output.new_paragraphs,
        output.new_tables
    );
    let _ = writeln!(md, "- 耗时：{:.2}s", output.elapsed.as_secs_f64());
    let _ = writeln!(md, "- 语义匹配：{}", semantic_label(&output.semantic));
    let _ = writeln!(md);

    let _ = writeln!(md, "## 汇总");
    let _ = writeln!(md);
    let _ = writeln!(md, "| 未变 | 修改 | 疑似修改 | 新增 | 删除 | 移动 |");
    let _ = writeln!(md, "| ---: | ---: | ---: | ---: | ---: | ---: |");
    let _ = writeln!(
        md,
        "| {} | {} | {} | {} | {} | {} |",
        output.summary.unchanged,
        output.summary.modified,
        output.summary.suspected_modified,
        output.summary.added,
        output.summary.removed,
        output.summary.moved
    );
    let _ = writeln!(md);

    let _ = writeln!(md, "## 确定差异");
    let _ = writeln!(md);
    let confirmed = confirmed_records(output);
    if confirmed.is_empty() {
        let _ = writeln!(md, "无");
        let _ = writeln!(md);
    } else {
        for record in confirmed {
            render_record_markdown(&mut md, record);
        }
    }

    let _ = writeln!(md, "## 疑似修改待确认");
    let _ = writeln!(md);
    let suspected = suspected_records(output);
    if suspected.is_empty() {
        let _ = writeln!(md, "无");
    } else {
        for record in suspected {
            render_record_markdown(&mut md, record);
        }
    }
    md
}

fn render_record_markdown(md: &mut String, record: &Record) {
    let _ = writeln!(
        md,
        "### {}　[{}] 旧 {} → 新 {}",
        record.id,
        record.status.label_zh(),
        position_label(record, true),
        position_label(record, false)
    );
    let _ = writeln!(md);
    let semantic = record
        .scores
        .semantic
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "—".to_string());
    let _ = writeln!(
        md,
        "- 文本相似度：{:.4}　语义相似度：{}　综合分：{:.4}",
        record.scores.text, semantic, record.scores.total
    );
    let _ = writeln!(
        md,
        "- matched_to：{}",
        record
            .matched_to
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".to_string())
    );
    if let Some(note) = &record.note {
        let _ = writeln!(md, "- 备注：{}", note);
    }
    let _ = writeln!(md);

    match record.status {
        Status::Modified | Status::SuspectedModified => {
            if let Some(diff) = &record.diff {
                let _ = writeln!(md, "**旧文（删除线为旧内容）**");
                let _ = writeln!(md);
                let _ = writeln!(md, "{}", render_diff_markdown(&diff.old));
                let _ = writeln!(md);
                let _ = writeln!(md, "**新文（加粗下划线为新增内容）**");
                let _ = writeln!(md);
                let _ = writeln!(md, "{}", render_diff_markdown(&diff.new));
                let _ = writeln!(md);
            }
        }
        Status::Added => {
            if let Some(text) = &record.new_text {
                let _ = writeln!(md, "**新增内容**");
                let _ = writeln!(md);
                let _ = writeln!(md, "{}", fenced_text(text));
                let _ = writeln!(md);
            }
        }
        Status::Removed => {
            if let Some(text) = &record.old_text {
                let _ = writeln!(md, "**删除内容**");
                let _ = writeln!(md);
                let _ = writeln!(md, "{}", fenced_text(text));
                let _ = writeln!(md);
            }
        }
        Status::Moved => {
            if let Some(text) = &record.new_text {
                let _ = writeln!(md, "**移动内容**");
                let _ = writeln!(md);
                let _ = writeln!(md, "{}", fenced_text(text));
                let _ = writeln!(md);
            }
        }
        Status::Unchanged => {}
    }
}

fn render_diff_markdown(spans: &[crate::result::DiffSpan]) -> String {
    let mut text = String::new();
    for span in spans {
        match span.op {
            DiffOp::Equal => text.push_str(&escape_html(&span.text)),
            DiffOp::Delete => {
                let _ = write!(text, "<del>{}</del>", escape_html(&span.text));
            }
            DiffOp::Insert => {
                let _ = write!(text, "<u>{}</u>", escape_html(&span.text));
            }
        }
    }
    text.replace('\n', "<br>")
}

fn fenced_text(text: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}text\n{text}\n{fence}")
}

fn position_label(record: &Record, old: bool) -> String {
    if let Some(table) = &record.table {
        return table.label(old);
    }
    let index = if old {
        record.old_para_index
    } else {
        record.new_para_index
    };
    index
        .map(|value| format!("§{value}"))
        .unwrap_or_else(|| "—".to_string())
}

fn format_text_html(text: &str) -> String {
    escape_html(text).replace('\n', "<br>")
}

pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape_html("<script>\"x\"&'y'</script>"),
            "&lt;script&gt;&quot;x&quot;&amp;&#39;y&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn no_unescaped_script_in_report() {
        let record = Record {
            id: 1,
            status: Status::Added,
            old_para_index: None,
            new_para_index: Some(1),
            old_text: None,
            new_text: Some("<script>alert(1)</script>".to_string()),
            scores: crate::result::Scores {
                text: 0.0,
                semantic: None,
                total: 0.0,
            },
            diff: None,
            matched_to: None,
            note: None,
            table: None,
            order: 0.0,
        };
        let output = CompareOutput {
            old_path: PathBuf::from("old.docx"),
            new_path: PathBuf::from("new.docx"),
            old_paragraphs: 0,
            new_paragraphs: 1,
            old_tables: 0,
            new_tables: 0,
            records: vec![record],
            summary: Summary {
                added: 1,
                ..Summary::default()
            },
            semantic: SemanticStatus::disabled("测试"),
            elapsed: std::time::Duration::from_millis(1),
        };
        let html = render_html(&output);
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>alert"));
        let md = render_markdown(&output);
        // Markdown 中的代码围栏内容按字面渲染，不会执行其中的 HTML。
        assert!(md.contains("```text\n<script>alert(1)</script>\n```"));
    }
}
