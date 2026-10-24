use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::normalize::normalize_for_match;

#[derive(Debug, Clone)]
pub struct Paragraph {
    /// 过滤空段后的 0 起始序号。
    pub index: usize,
    pub text: String,
    pub normalized: String,
}

#[derive(Debug, Clone)]
pub struct TableRow {
    /// 表内 0 起始行号，空行已剔除。
    pub index: usize,
    /// 展开后的单元格文本；纵向合并已按内容复制到覆盖的每一行，横向合并只在首列保留内容。
    pub cells: Vec<String>,
    /// 展示用的一行文本：单元格以 " | " 连接。
    pub text: String,
    pub normalized: String,
    /// 首列归一化文本，非空时作为行主键。
    pub key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Table {
    /// 全文 0 起始表号。
    pub index: usize,
    /// 紧邻表格上方的标题段（如“表1 指标完成情况”），仅用于展示。
    pub caption: Option<String>,
    /// 表格之前的非空段落数，用于把表格记录插回正文顺序。
    pub anchor: usize,
    pub rows: Vec<TableRow>,
}

#[derive(Debug, Clone, Default)]
pub struct Document {
    pub paragraphs: Vec<Paragraph>,
    pub tables: Vec<Table>,
}

/// 解析 .docx：读取 word/document.xml 正文的段落与表格，跳过页眉页脚、脚注批注。
pub fn parse(path: &Path) -> Result<Document> {
    if !path.exists() {
        bail!("文件不存在：{}", path.display());
    }
    if !path.is_file() {
        bail!("不是文件：{}", path.display());
    }
    let is_docx = path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("docx"));
    if !is_docx {
        bail!("仅支持 .docx 文件：{}", path.display());
    }

    let file = File::open(path).with_context(|| format!("无法打开文件：{}", path.display()))?;
    let reader = BufReader::new(file);
    let mut archive = zip::ZipArchive::new(reader)
        .with_context(|| format!("文件损坏或不是有效的 .docx：{}", path.display()))?;

    let xml = read_document_xml(&mut archive)
        .with_context(|| format!("读取正文失败：{}", path.display()))?;
    let (raw_paragraphs, raw_tables) =
        extract(&xml).with_context(|| format!("解析正文失败：{}", path.display()))?;

    let paragraphs: Vec<Paragraph> = raw_paragraphs
        .into_iter()
        .enumerate()
        .map(|(index, text)| Paragraph {
            index,
            normalized: normalize_for_match(&text),
            text,
        })
        .collect();

    let mut tables = Vec::new();
    for raw in raw_tables {
        let caption = raw
            .anchor
            .checked_sub(1)
            .and_then(|index| paragraphs.get(index))
            .filter(|paragraph| looks_like_caption(&paragraph.text))
            .map(|paragraph| paragraph.text.trim().to_string());
        let mut rows: Vec<TableRow> = Vec::new();
        for cells in raw.rows {
            let text = row_text(&cells);
            if text.trim().is_empty() {
                continue;
            }
            let normalized = normalize_for_match(&text);
            let key = cells
                .first()
                .map(|cell| normalize_for_match(cell))
                .filter(|key| !key.is_empty());
            rows.push(TableRow {
                index: rows.len(),
                cells,
                text,
                normalized,
                key,
            });
        }
        if rows.is_empty() {
            continue;
        }
        tables.push(Table {
            index: tables.len(),
            caption,
            anchor: raw.anchor,
            rows,
        });
    }

    Ok(Document { paragraphs, tables })
}

fn read_document_xml<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>) -> Result<String> {
    let mut entry = archive
        .by_name("word/document.xml")
        .context("缺少 word/document.xml，可能不是 Word 文档")?;
    let mut xml = String::new();
    entry
        .read_to_string(&mut xml)
        .context("word/document.xml 不是有效的 UTF-8 文本")?;
    Ok(xml)
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn attr_value(start: &BytesStart, name: &str) -> Option<String> {
    for attr in start.attributes().flatten() {
        let key = attr.key.as_ref();
        if local_name(key) == name {
            return Some(attr.value.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum VMerge {
    #[default]
    None,
    Restart,
    Continue,
}

#[derive(Debug, Clone)]
struct RawCell {
    text: String,
    span: usize,
    vmerge: VMerge,
}

impl RawCell {
    fn new() -> Self {
        Self {
            text: String::new(),
            span: 1,
            vmerge: VMerge::None,
        }
    }
}

struct RawTable {
    anchor: usize,
    rows: Vec<Vec<String>>,
}

fn row_text(cells: &[String]) -> String {
    let mut end = cells.len();
    while end > 0 && cells[end - 1].trim().is_empty() {
        end -= 1;
    }
    cells[..end]
        .iter()
        .map(|cell| cell.trim())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 纵向合并的续格按上一行同列内容展开到当前行；横向合并只在起始列保留文本。
fn expand_row(cells: &[RawCell], previous: Option<&Vec<String>>) -> Vec<String> {
    let mut expanded = Vec::new();
    let mut column = 0usize;
    for cell in cells {
        while expanded.len() < column {
            expanded.push(String::new());
        }
        let text = if cell.vmerge == VMerge::Continue {
            previous
                .and_then(|row| row.get(column))
                .cloned()
                .unwrap_or_default()
        } else {
            cell.text.clone()
        };
        expanded.push(text);
        let span = cell.span.max(1);
        for _ in 1..span {
            expanded.push(String::new());
        }
        column += span;
    }
    expanded
}

fn looks_like_caption(text: &str) -> bool {
    let text = text.trim();
    if text.chars().count() > 60 {
        return false;
    }
    let mut chars = text.chars();
    match chars.next() {
        Some('表') => chars
            .next()
            .is_some_and(|ch| ch.is_ascii_digit() || ch.is_whitespace()),
        Some('附') | Some('续') => chars.next() == Some('表'),
        _ => false,
    }
}

fn extract(xml: &str) -> Result<(Vec<String>, Vec<RawTable>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;

    let mut paragraphs: Vec<String> = Vec::new();
    let mut tables: Vec<RawTable> = Vec::new();
    let mut table_depth: usize = 0;
    let mut paragraph_depth: usize = 0;
    let mut current = String::new();
    let mut in_text = false;
    let mut in_deleted_text = false;

    let mut table_anchor = 0usize;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Option<Vec<RawCell>> = None;
    let mut cell: Option<RawCell> = None;
    let mut cell_paragraphs: Vec<String> = Vec::new();
    let mut cell_current = String::new();
    let mut cell_paragraph_depth: usize = 0;

    loop {
        let event = reader
            .read_event()
            .context("word/document.xml XML 结构损坏")?;
        match event {
            Event::Eof => break,
            Event::Start(start) => {
                let qname = start.name();
                let name = local_name(qname.as_ref());
                match name {
                    "tbl" => {
                        if table_depth == 0 {
                            table_anchor = paragraphs.len();
                            rows.clear();
                        }
                        table_depth += 1;
                    }
                    "tr" if table_depth == 1 => {
                        row = Some(Vec::new());
                    }
                    "tc" if table_depth == 1 && row.is_some() => {
                        cell = Some(RawCell::new());
                        cell_paragraphs.clear();
                        cell_current.clear();
                        cell_paragraph_depth = 0;
                    }
                    "gridSpan" if table_depth == 1 && cell.is_some() => {
                        if let Some(span) = attr_value(&start, "val").and_then(|v| v.parse().ok()) {
                            if let Some(cell) = cell.as_mut() {
                                cell.span = span;
                            }
                        }
                    }
                    "vMerge" if table_depth == 1 && cell.is_some() => {
                        if let Some(cell) = cell.as_mut() {
                            cell.vmerge = match attr_value(&start, "val").as_deref() {
                                Some("restart") => VMerge::Restart,
                                _ => VMerge::Continue,
                            };
                        }
                    }
                    "p" if table_depth == 0 => {
                        if paragraph_depth == 0 {
                            current.clear();
                        }
                        paragraph_depth += 1;
                    }
                    "p" if table_depth == 1 && cell.is_some() => {
                        if cell_paragraph_depth == 0 {
                            cell_current.clear();
                        }
                        cell_paragraph_depth += 1;
                    }
                    "t" if (table_depth == 0 && paragraph_depth > 0)
                        || (table_depth == 1 && cell_paragraph_depth > 0) =>
                    {
                        in_text = true
                    }
                    "delText"
                        if (table_depth == 0 && paragraph_depth > 0)
                            || (table_depth == 1 && cell_paragraph_depth > 0) =>
                    {
                        in_deleted_text = true
                    }
                    "tab" => push_char(
                        '\t',
                        table_depth,
                        paragraph_depth,
                        cell_paragraph_depth,
                        &mut current,
                        &mut cell_current,
                    ),
                    "br" | "cr" => push_char(
                        '\n',
                        table_depth,
                        paragraph_depth,
                        cell_paragraph_depth,
                        &mut current,
                        &mut cell_current,
                    ),
                    "noBreakHyphen" => push_char(
                        '-',
                        table_depth,
                        paragraph_depth,
                        cell_paragraph_depth,
                        &mut current,
                        &mut cell_current,
                    ),
                    _ => {}
                }
            }
            Event::End(end) => {
                let qname = end.name();
                let name = local_name(qname.as_ref());
                match name {
                    "tbl" => {
                        if table_depth > 0 {
                            table_depth -= 1;
                        }
                        if table_depth == 0 {
                            if !rows.is_empty() {
                                tables.push(RawTable {
                                    anchor: table_anchor,
                                    rows: std::mem::take(&mut rows),
                                });
                            }
                            row = None;
                            cell = None;
                            cell_paragraphs.clear();
                            cell_current.clear();
                            cell_paragraph_depth = 0;
                        }
                    }
                    "tr" if table_depth == 1 => {
                        if let Some(raw_row) = row.take() {
                            let expanded = expand_row(&raw_row, rows.last());
                            if expanded.iter().any(|value| !value.trim().is_empty()) {
                                rows.push(expanded);
                            }
                        }
                    }
                    "tc" if table_depth == 1 => {
                        if let Some(mut raw_cell) = cell.take() {
                            raw_cell.text = cell_paragraphs.join("\n").trim().to_string();
                            if let Some(row) = row.as_mut() {
                                row.push(raw_cell);
                            }
                        }
                        cell_paragraphs.clear();
                        cell_current.clear();
                        cell_paragraph_depth = 0;
                    }
                    "p" if table_depth == 0 => {
                        paragraph_depth = paragraph_depth.saturating_sub(1);
                        if paragraph_depth == 0 {
                            let text = std::mem::take(&mut current);
                            if !text.trim().is_empty() {
                                paragraphs.push(text);
                            }
                        }
                    }
                    "p" if table_depth == 1 && cell_paragraph_depth > 0 => {
                        cell_paragraph_depth -= 1;
                        if cell_paragraph_depth == 0 {
                            let text = std::mem::take(&mut cell_current);
                            if !text.trim().is_empty() {
                                cell_paragraphs.push(text);
                            }
                        }
                    }
                    "t" => in_text = false,
                    "delText" => in_deleted_text = false,
                    _ => {}
                }
            }
            Event::Text(text) => {
                if in_text && !in_deleted_text {
                    push_str(
                        &text.xml10_content(),
                        table_depth,
                        paragraph_depth,
                        cell_paragraph_depth,
                        &mut current,
                        &mut cell_current,
                    );
                }
            }
            Event::GeneralRef(reference) => {
                if in_text && !in_deleted_text {
                    if reference.is_char_ref() {
                        if let Ok(Some(ch)) = reference.resolve_char_ref() {
                            push_str(
                                &ch.to_string(),
                                table_depth,
                                paragraph_depth,
                                cell_paragraph_depth,
                                &mut current,
                                &mut cell_current,
                            );
                        }
                    } else if let Some(value) =
                        quick_xml::escape::resolve_predefined_entity(&reference)
                    {
                        push_str(
                            value,
                            table_depth,
                            paragraph_depth,
                            cell_paragraph_depth,
                            &mut current,
                            &mut cell_current,
                        );
                    }
                }
            }
            Event::CData(text) => {
                if in_text && !in_deleted_text {
                    push_str(
                        &text.xml10_content(),
                        table_depth,
                        paragraph_depth,
                        cell_paragraph_depth,
                        &mut current,
                        &mut cell_current,
                    );
                }
            }
            _ => {}
        }
    }

    Ok((paragraphs, tables))
}

fn push_char(
    ch: char,
    table_depth: usize,
    paragraph_depth: usize,
    cell_paragraph_depth: usize,
    current: &mut String,
    cell_current: &mut String,
) {
    if table_depth == 0 && paragraph_depth > 0 {
        current.push(ch);
    } else if table_depth == 1 && cell_paragraph_depth > 0 {
        cell_current.push(ch);
    }
}

fn push_str(
    value: &str,
    table_depth: usize,
    paragraph_depth: usize,
    cell_paragraph_depth: usize,
    current: &mut String,
    cell_current: &mut String,
) {
    if table_depth == 0 && paragraph_depth > 0 {
        current.push_str(value);
    } else if table_depth == 1 && cell_paragraph_depth > 0 {
        cell_current.push_str(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_xml(xml: &str) -> (Vec<String>, Vec<RawTable>) {
        extract(xml).unwrap()
    }

    #[test]
    fn extracts_paragraphs_and_tables_in_order() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p><w:pPr><w:pStyle w:val="1"/></w:pPr><w:r><w:t>表1 指标情况</w:t></w:r></w:p>
  <w:p><w:r><w:t xml:space="preserve">   </w:t></w:r></w:p>
  <w:tbl><w:tr><w:tc><w:p><w:r><w:t>指标</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>本月</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
  <w:p><w:r><w:t>第二</w:t></w:r><w:r><w:t>段</w:t></w:r><w:r><w:tab/><w:t>含制表</w:t></w:r></w:p>
  <w:p/>
</w:body>
</w:document>"#;
        let (paras, tables) = extract_xml(xml);
        assert_eq!(paras, vec!["表1 指标情况", "第二段\t含制表"]);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].anchor, 1);
        assert_eq!(tables[0].rows, vec![vec!["指标", "本月"]]);
    }

    #[test]
    fn skips_tracked_deletions() {
        let xml = r#"<w:document xmlns:w="x"><w:body>
<w:p><w:r><w:t>保留</w:t></w:r><w:del><w:r><w:delText>删掉</w:delText></w:r></w:del><w:ins><w:r><w:t>插入</w:t></w:r></w:ins></w:p>
</w:body></w:document>"#;
        let (paras, tables) = extract_xml(xml);
        assert_eq!(paras, vec!["保留插入"]);
        assert!(tables.is_empty());
    }

    #[test]
    fn keeps_manual_line_break_inside_paragraph() {
        let xml = r#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>上行</w:t><w:br/><w:t>下行</w:t></w:r></w:p></w:body></w:document>"#;
        let (paras, _) = extract_xml(xml);
        assert_eq!(paras, vec!["上行\n下行"]);
    }

    #[test]
    fn vertical_merge_repeats_content_into_each_row() {
        let xml = r#"<w:document xmlns:w="x"><w:body><w:tbl>
<w:tr>
  <w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>合并</w:t></w:r></w:p></w:tc>
  <w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc>
</w:tr>
<w:tr>
  <w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p/></w:tc>
  <w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc>
</w:tr>
<w:tr>
  <w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>另一组</w:t></w:r></w:p></w:tc>
  <w:tc><w:p><w:r><w:t>C</w:t></w:r></w:p></w:tc>
</w:tr>
</w:tbl></w:body></w:document>"#;
        let (_, tables) = extract_xml(xml);
        assert_eq!(tables[0].rows.len(), 3);
        assert_eq!(tables[0].rows[0], vec!["合并", "A"]);
        assert_eq!(tables[0].rows[1], vec!["合并", "B"]);
        assert_eq!(tables[0].rows[2], vec!["另一组", "C"]);
    }

    #[test]
    fn horizontal_merge_keeps_placeholder_columns() {
        let xml = r#"<w:document xmlns:w="x"><w:body><w:tbl>
<w:tr>
  <w:tc><w:tcPr><w:gridSpan w:val="3"/></w:tcPr><w:p><w:r><w:t>合并表头</w:t></w:r></w:p></w:tc>
</w:tr>
<w:tr><w:tc><w:p><w:r><w:t>甲</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>乙</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>丙</w:t></w:r></w:p></w:tc></w:tr>
</w:tbl></w:body></w:document>"#;
        let (_, tables) = extract_xml(xml);
        assert_eq!(tables[0].rows[0], vec!["合并表头", "", ""]);
        assert_eq!(tables[0].rows[1], vec!["甲", "乙", "丙"]);
    }

    #[test]
    fn nested_table_text_stays_out_of_outer_cells() {
        let xml = r#"<w:document xmlns:w="x"><w:body><w:tbl>
<w:tr><w:tc>
  <w:p><w:r><w:t>外层</w:t></w:r></w:p>
  <w:tbl><w:tr><w:tc><w:p><w:r><w:t>嵌套</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:tc></w:tr>
</w:tbl></w:body></w:document>"#;
        let (_, tables) = extract_xml(xml);
        assert_eq!(tables[0].rows, vec![vec!["外层"]]);
    }

    #[test]
    fn detects_caption_above_table() {
        let xml = r#"<w:document xmlns:w="x"><w:body>
<w:p><w:r><w:t>表决通过的事项如下。</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>甲</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:p><w:r><w:t>附表2 人员名单</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>乙</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body></w:document>"#;
        let dir = std::env::temp_dir();
        let path = dir.join("diffract-caption-test.docx");
        write_docx(&path, xml);
        let document = parse(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(document.tables.len(), 2);
        assert_eq!(document.tables[0].caption, None);
        assert_eq!(
            document.tables[1].caption.as_deref(),
            Some("附表2 人员名单")
        );
    }

    fn write_docx(path: &Path, xml: &str) {
        use std::io::Write;
        let file = File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        archive.start_file("word/document.xml", options).unwrap();
        archive.write_all(xml.as_bytes()).unwrap();
        archive.finish().unwrap();
    }
}
