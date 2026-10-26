use std::time::Instant;

use anyhow::Result;

use crate::config::{Config, XlsxCompareMode};
use crate::diff::para_diff;
use crate::matching::Progress;
use crate::result::Status;

use super::align::{ColumnPair, RowPair, align_sheets};
use super::model::{Cell, CellValue, Sheet, Workbook, address, column_label};
use super::reader::read_workbook;
use super::result::{ColumnInfo, GridCell, GridRow, SheetDiff, XlsxOutput, XlsxSummary};

pub fn compare_files(
    old_path: &std::path::Path,
    new_path: &std::path::Path,
    config: &Config,
    progress: &mut dyn FnMut(Progress),
) -> Result<XlsxOutput> {
    let started = Instant::now();
    progress(Progress::Stage("读取 Excel 工作簿".to_string()));
    let old = read_workbook(old_path)?;
    let new = read_workbook(new_path)?;
    let mut old = old;
    let mut new = new;
    apply_header_overrides(&mut old, config);
    apply_header_overrides(&mut new, config);
    log::info!(
        "Excel 解析完成：旧 {} 个工作表，新 {} 个工作表",
        old.sheets.len(),
        new.sheets.len()
    );

    progress(Progress::Stage("对齐工作表与单元格".to_string()));
    let mut summary = XlsxSummary::default();
    let mut sheets = Vec::new();
    let mut used_new = vec![false; new.sheets.len()];

    for old_sheet in &old.sheets {
        if let Some((index, new_sheet)) = new
            .sheets
            .iter()
            .enumerate()
            .find(|(index, sheet)| !used_new[*index] && sheet.name == old_sheet.name)
        {
            used_new[index] = true;
            sheets.push(compare_sheet(
                old_sheet,
                new_sheet,
                config,
                old_sheet.header_row,
                new_sheet.header_row,
                &mut summary,
            ));
        } else {
            sheets.push(sheet_one_side(
                old_sheet,
                Status::Removed,
                old_sheet.header_row,
                &mut summary,
            ));
        }
    }
    for (index, new_sheet) in new.sheets.iter().enumerate() {
        if !used_new[index] {
            sheets.push(sheet_one_side(
                new_sheet,
                Status::Added,
                new_sheet.header_row,
                &mut summary,
            ));
        }
    }

    let elapsed = started.elapsed();
    let old_sheet_count = old.sheets.len();
    let new_sheet_count = new.sheets.len();
    log::info!(
        "Excel 比较完成：工作表 {}，单元格差异 {}，用时 {:.2}s",
        sheets.len(),
        summary.cell_differences(),
        elapsed.as_secs_f64()
    );

    Ok(XlsxOutput {
        old_path: old_path.to_path_buf(),
        new_path: new_path.to_path_buf(),
        old_sheet_count,
        new_sheet_count,
        sheets,
        summary,
        elapsed,
        note: config
            .xlsx
            .semantic_enabled
            .then(|| "首版 Excel 引擎未启用语义匹配，已按值与公式比较".to_string()),
    })
}

fn apply_header_overrides(workbook: &mut Workbook, config: &Config) {
    for sheet in &mut workbook.sheets {
        sheet.header_row = match config.xlsx.header_row {
            Some(0) => None,
            Some(row) => Some(row),
            None => sheet.header_row,
        };
    }
}

fn compare_sheet(
    old: &Sheet,
    new: &Sheet,
    config: &Config,
    old_header_row: Option<usize>,
    new_header_row: Option<usize>,
    summary: &mut XlsxSummary,
) -> SheetDiff {
    let header_row = new_header_row.or(old_header_row);
    let alignment = align_sheets(old, new, &config.xlsx, old_header_row, new_header_row);
    let mode = config.xlsx.compare_mode;
    let mut rows = Vec::with_capacity(alignment.rows.len());
    let mut column_modified = vec![false; alignment.columns.len()];

    for row_pair in &alignment.rows {
        let is_title_row = row_pair
            .old
            .is_some_and(|row| old_header_row.is_some_and(|header| row < header))
            || row_pair
                .new
                .is_some_and(|row| new_header_row.is_some_and(|header| row < header));
        let mut cells = Vec::with_capacity(alignment.columns.len());
        for (column_index, column_pair) in alignment.columns.iter().enumerate() {
            let cell = build_cell(old, new, row_pair, column_pair, mode, summary);
            if !is_title_row && cell.status == Status::Modified {
                column_modified[column_index] = true;
            }
            cells.push(cell);
        }
        let status = row_status(row_pair, &cells);
        match status {
            Status::Added => summary.rows_added += 1,
            Status::Removed => summary.rows_removed += 1,
            Status::Moved => summary.rows_moved += 1,
            Status::Modified => summary.rows_modified += 1,
            _ => {}
        }
        rows.push(GridRow {
            old_row: row_pair.old,
            new_row: row_pair.new,
            status,
            cells,
        });
    }

    if old_header_row.is_some() || new_header_row.is_some() {
        let row_pair = RowPair {
            old: old_header_row,
            new: new_header_row,
            moved: false,
        };
        let mut cells = Vec::with_capacity(alignment.columns.len());
        for (column_index, column_pair) in alignment.columns.iter().enumerate() {
            let cell = build_cell(old, new, &row_pair, column_pair, mode, summary);
            if cell.status == Status::Modified {
                column_modified[column_index] = true;
            }
            cells.push(cell);
        }
        let status = if cells.iter().any(|cell| cell.status == Status::Modified) {
            Status::Modified
        } else {
            Status::Unchanged
        };
        rows.insert(
            0,
            GridRow {
                old_row: old_header_row,
                new_row: new_header_row,
                status,
                cells,
            },
        );
    }

    let columns: Vec<ColumnInfo> = alignment
        .columns
        .iter()
        .enumerate()
        .map(|(index, pair)| {
            let status = if pair.old.is_none() {
                summary.columns_added += 1;
                Status::Added
            } else if pair.new.is_none() {
                summary.columns_removed += 1;
                Status::Removed
            } else if column_modified[index] {
                summary.columns_modified += 1;
                Status::Modified
            } else if pair.moved {
                summary.columns_moved += 1;
                Status::Moved
            } else {
                Status::Unchanged
            };
            ColumnInfo {
                label: column_label_text(old, new, pair, old_header_row, new_header_row),
                old_col: pair.old,
                new_col: pair.new,
                status,
            }
        })
        .collect();

    let status = sheet_status(&columns, &rows);
    if status == Status::Modified {
        summary.sheets_modified += 1;
    }
    SheetDiff {
        name: new.name.clone(),
        status,
        columns,
        rows,
        header_row,
    }
}

fn sheet_one_side(
    sheet: &Sheet,
    status: Status,
    header_row: Option<usize>,
    summary: &mut XlsxSummary,
) -> SheetDiff {
    let columns_used = sheet.used_columns();
    let columns: Vec<ColumnInfo> = columns_used
        .iter()
        .map(|column| ColumnInfo {
            label: header_label(sheet, header_row, *column),
            old_col: (status == Status::Removed).then_some(*column),
            new_col: (status == Status::Added).then_some(*column),
            status,
        })
        .collect();
    let rows: Vec<GridRow> = sheet
        .rows
        .iter()
        .map(|row| {
            let cells: Vec<GridCell> = columns_used
                .iter()
                .map(|column| {
                    let cell = row.cell(*column);
                    let non_empty = cell.is_some_and(|cell| !cell.is_empty());
                    if non_empty {
                        if status == Status::Added {
                            summary.cells_added += 1;
                        } else {
                            summary.cells_removed += 1;
                        }
                    }
                    GridCell {
                        status: if non_empty { status } else { Status::Unchanged },
                        old_col: (status == Status::Removed).then_some(*column),
                        new_col: (status == Status::Added).then_some(*column),
                        old_address: (status == Status::Removed)
                            .then(|| address(row.number, *column)),
                        new_address: (status == Status::Added)
                            .then(|| address(row.number, *column)),
                        old_value: if status == Status::Removed {
                            cell.map(|cell| cell.value.clone())
                        } else {
                            None
                        },
                        new_value: if status == Status::Added {
                            cell.map(|cell| cell.value.clone())
                        } else {
                            None
                        },
                        old_formula: if status == Status::Removed {
                            cell.and_then(|cell| cell.formula.clone())
                        } else {
                            None
                        },
                        new_formula: if status == Status::Added {
                            cell.and_then(|cell| cell.formula.clone())
                        } else {
                            None
                        },
                        value_changed: false,
                        formula_changed: false,
                        type_changed: false,
                        diff: None,
                        note: None,
                    }
                })
                .collect();
            GridRow {
                old_row: (status == Status::Removed).then_some(row.number),
                new_row: (status == Status::Added).then_some(row.number),
                status,
                cells,
            }
        })
        .collect();

    if status == Status::Added {
        summary.sheets_added += 1;
        summary.rows_added += rows.len();
        summary.columns_added += columns.len();
    } else {
        summary.sheets_removed += 1;
        summary.rows_removed += rows.len();
        summary.columns_removed += columns.len();
    }

    SheetDiff {
        name: sheet.name.clone(),
        status,
        columns,
        rows,
        header_row,
    }
}

fn build_cell(
    old: &Sheet,
    new: &Sheet,
    row_pair: &RowPair,
    column_pair: &ColumnPair,
    mode: XlsxCompareMode,
    summary: &mut XlsxSummary,
) -> GridCell {
    let old_cell = match (row_pair.old, column_pair.old) {
        (Some(row), Some(column)) => old.cell(row, column),
        _ => None,
    };
    let new_cell = match (row_pair.new, column_pair.new) {
        (Some(row), Some(column)) => new.cell(row, column),
        _ => None,
    };
    let old_non_empty = old_cell.is_some_and(|cell| !cell.is_empty());
    let new_non_empty = new_cell.is_some_and(|cell| !cell.is_empty());
    let both_rows = row_pair.old.is_some() && row_pair.new.is_some();
    let both_columns = column_pair.old.is_some() && column_pair.new.is_some();

    let (status, value_changed, formula_changed, type_changed, diff) = if !both_rows
        || !both_columns
    {
        let exists = old_non_empty || new_non_empty;
        let status = if !exists {
            Status::Unchanged
        } else if row_pair.new.is_none() || column_pair.new.is_none() {
            Status::Removed
        } else {
            Status::Added
        };
        (status, false, false, false, None)
    } else {
        let old_cell = old_cell.filter(|cell| !cell.is_empty());
        let new_cell = new_cell.filter(|cell| !cell.is_empty());
        let value_changed = match mode {
            XlsxCompareMode::Value | XlsxCompareMode::Both => !values_equal(
                old_cell.map(|cell| &cell.value),
                new_cell.map(|cell| &cell.value),
            ),
            XlsxCompareMode::Formula => false,
        };
        let formula_changed = match mode {
            XlsxCompareMode::Formula | XlsxCompareMode::Both => !formulas_equal(old_cell, new_cell),
            XlsxCompareMode::Value => false,
        };
        let type_changed = old_cell
            .zip(new_cell)
            .is_some_and(|(old, new)| !types_equal(&old.value, &new.value));
        let changed = value_changed || formula_changed;
        let status = if changed {
            Status::Modified
        } else if row_pair.moved || column_pair.moved {
            Status::Moved
        } else {
            Status::Unchanged
        };
        let diff = if value_changed
            && let (
                Some(Cell {
                    value: CellValue::Text(old_text),
                    ..
                }),
                Some(Cell {
                    value: CellValue::Text(new_text),
                    ..
                }),
            ) = (old_cell, new_cell)
        {
            Some(para_diff(old_text, new_text))
        } else {
            None
        };
        (status, value_changed, formula_changed, type_changed, diff)
    };

    let note = if status == Status::Moved {
        match (row_pair.moved, column_pair.moved) {
            (true, true) => Some("行与列均判定为移动".to_string()),
            (true, false) => Some("行判定为移动".to_string()),
            (false, true) => Some("列判定为移动".to_string()),
            (false, false) => None,
        }
    } else {
        None
    };

    match status {
        Status::Modified => summary.cells_modified += 1,
        Status::Added => summary.cells_added += 1,
        Status::Removed => summary.cells_removed += 1,
        Status::Moved => summary.cells_moved += 1,
        Status::Unchanged if old_non_empty || new_non_empty => summary.cells_unchanged += 1,
        _ => {}
    }

    GridCell {
        status,
        old_col: column_pair.old,
        new_col: column_pair.new,
        old_address: row_pair
            .old
            .zip(column_pair.old)
            .map(|(row, column)| address(row, column)),
        new_address: row_pair
            .new
            .zip(column_pair.new)
            .map(|(row, column)| address(row, column)),
        old_value: old_cell.map(|cell| cell.value.clone()),
        new_value: new_cell.map(|cell| cell.value.clone()),
        old_formula: old_cell.and_then(|cell| cell.formula.clone()),
        new_formula: new_cell.and_then(|cell| cell.formula.clone()),
        value_changed,
        formula_changed,
        type_changed,
        diff,
        note,
    }
}

fn values_equal(old: Option<&CellValue>, new: Option<&CellValue>) -> bool {
    match (old, new) {
        (None, None) | (Some(CellValue::Empty), None) | (None, Some(CellValue::Empty)) => true,
        (Some(CellValue::Empty), Some(CellValue::Empty)) => true,
        (Some(old), Some(new)) => match (numeric_value(old), numeric_value(new)) {
            // calamine 会把同一个 Excel 数值拆成 Int 或 Float，比较时不能当成类型变化。
            (Some(old), Some(new)) => old == new,
            _ => old == new,
        },
        _ => false,
    }
}

fn numeric_value(value: &CellValue) -> Option<f64> {
    match value {
        CellValue::Integer(value) => Some(*value as f64),
        CellValue::Float(value) => Some(*value),
        _ => None,
    }
}

fn types_equal(old: &CellValue, new: &CellValue) -> bool {
    old.same_type(new) || (numeric_value(old).is_some() && numeric_value(new).is_some())
}

fn formulas_equal(old: Option<&Cell>, new: Option<&Cell>) -> bool {
    normalize_formula(old.and_then(|cell| cell.formula.as_deref()))
        == normalize_formula(new.and_then(|cell| cell.formula.as_deref()))
}

fn normalize_formula(formula: Option<&str>) -> Option<String> {
    formula.map(|formula| formula.trim().trim_start_matches('=').to_string())
}

fn row_status(pair: &RowPair, cells: &[GridCell]) -> Status {
    if pair.new.is_none() {
        return Status::Removed;
    }
    if pair.old.is_none() {
        return Status::Added;
    }
    if cells.iter().any(|cell| cell.status == Status::Modified) {
        Status::Modified
    } else if pair.moved {
        Status::Moved
    } else {
        Status::Unchanged
    }
}

fn sheet_status(columns: &[ColumnInfo], rows: &[GridRow]) -> Status {
    if columns.iter().any(|column| {
        matches!(
            column.status,
            Status::Added | Status::Removed | Status::Modified | Status::Moved
        )
    }) || rows.iter().any(|row| {
        matches!(
            row.status,
            Status::Added | Status::Removed | Status::Modified | Status::Moved
        )
    }) {
        Status::Modified
    } else {
        Status::Unchanged
    }
}

fn column_label_text(
    old: &Sheet,
    new: &Sheet,
    pair: &ColumnPair,
    old_header_row: Option<usize>,
    new_header_row: Option<usize>,
) -> String {
    for (sheet, column, header_row) in [
        (new, pair.new, new_header_row),
        (old, pair.old, old_header_row),
    ] {
        if let Some(header_row) = header_row
            && let Some(column) = column
            && let Some(cell) = sheet.cell(header_row, column)
        {
            let label = cell.value.display().trim().to_string();
            if !label.is_empty() {
                return label;
            }
        }
    }
    column_label(pair.new.or(pair.old).unwrap_or(0))
}

fn header_label(sheet: &Sheet, header_row: Option<usize>, column: usize) -> String {
    if let Some(header_row) = header_row
        && let Some(cell) = sheet.cell(header_row, column)
    {
        let label = cell.value.display().trim().to_string();
        if !label.is_empty() {
            return label;
        }
    }
    column_label(column)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use zip::CompressionMethod;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::xlsx::model::column_label;

    enum Fx {
        Text(&'static str),
        Num(f64),
        Formula(&'static str, f64),
    }

    struct Fixture {
        path: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn fixture(name: &str, sheets: &[(&str, Vec<Vec<Fx>>)]) -> Fixture {
        fixture_with_merges(name, sheets, &[])
    }

    fn fixture_with_merges(
        name: &str,
        sheets: &[(&str, Vec<Vec<Fx>>)],
        merges: &[Vec<&str>],
    ) -> Fixture {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("xlsx-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}.xlsx", std::process::id()));
        write_xlsx(&path, sheets, merges);
        Fixture { path }
    }

    fn write_xlsx(path: &Path, sheets: &[(&str, Vec<Vec<Fx>>)], merges: &[Vec<&str>]) {
        let file = File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        zip.start_file("[Content_Types].xml", options).unwrap();
        zip.write_all(content_types(sheets.len()).as_bytes())
            .unwrap();

        zip.start_file("_rels/.rels", options).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
        )
        .unwrap();

        zip.start_file("xl/workbook.xml", options).unwrap();
        zip.write_all(workbook_xml(sheets).as_bytes()).unwrap();

        zip.start_file("xl/_rels/workbook.xml.rels", options)
            .unwrap();
        zip.write_all(workbook_rels(sheets.len()).as_bytes())
            .unwrap();

        for (index, (_, rows)) in sheets.iter().enumerate() {
            zip.start_file(format!("xl/worksheets/sheet{}.xml", index + 1), options)
                .unwrap();
            let sheet_merges = merges.get(index).map(Vec::as_slice).unwrap_or(&[]);
            zip.write_all(sheet_xml(rows, sheet_merges).as_bytes())
                .unwrap();
        }
        zip.finish().unwrap();
    }

    fn content_types(sheet_count: usize) -> String {
        let mut text = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
"#,
        );
        for index in 1..=sheet_count {
            text.push_str(&format!(
                "<Override PartName=\"/xl/worksheets/sheet{index}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\n"
            ));
        }
        text.push_str("</Types>");
        text
    }

    fn workbook_xml(sheets: &[(&str, Vec<Vec<Fx>>)]) -> String {
        let mut text = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets>
"#,
        );
        for (index, (name, _)) in sheets.iter().enumerate() {
            text.push_str(&format!(
                "<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rId{}\"/>\n",
                xml_escape(name),
                index + 1,
                index + 1
            ));
        }
        text.push_str("</sheets>\n</workbook>");
        text
    }

    fn workbook_rels(sheet_count: usize) -> String {
        let mut text = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
"#,
        );
        for index in 1..=sheet_count {
            text.push_str(&format!(
                "<Relationship Id=\"rId{index}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{index}.xml\"/>\n"
            ));
        }
        text.push_str("</Relationships>");
        text
    }

    fn sheet_xml(rows: &[Vec<Fx>], merges: &[&str]) -> String {
        let mut text = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
"#,
        );
        for (row_index, row) in rows.iter().enumerate() {
            let row_number = row_index + 1;
            text.push_str(&format!("<row r=\"{row_number}\">"));
            for (column_index, cell) in row.iter().enumerate() {
                let reference = format!("{}{}", column_label(column_index + 1), row_number);
                match cell {
                    Fx::Text(value) => text.push_str(&format!(
                        "<c r=\"{reference}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
                        xml_escape(value)
                    )),
                    Fx::Num(value) => {
                        text.push_str(&format!("<c r=\"{reference}\"><v>{value}</v></c>"));
                    }
                    Fx::Formula(formula, value) => text.push_str(&format!(
                        "<c r=\"{reference}\"><f>{}</f><v>{value}</v></c>",
                        xml_escape(formula)
                    )),
                }
            }
            text.push_str("</row>");
        }
        text.push_str("</sheetData>");
        if !merges.is_empty() {
            text.push_str(&format!("<mergeCells count=\"{}\">", merges.len()));
            for reference in merges {
                text.push_str(&format!("<mergeCell ref=\"{reference}\"/>"));
            }
            text.push_str("</mergeCells>");
        }
        text.push_str("</worksheet>");
        text
    }

    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    fn run(old: &Path, new: &Path, config: &Config) -> XlsxOutput {
        compare_files(old, new, config, &mut |_| {}).unwrap()
    }

    #[test]
    fn compares_cells_and_added_removed_rows() {
        let old = fixture(
            "cells-old",
            &[(
                "台账",
                vec![
                    vec![Fx::Text("姓名"), Fx::Text("部门"), Fx::Text("金额")],
                    vec![Fx::Text("张三"), Fx::Text("综合部"), Fx::Num(100.0)],
                    vec![Fx::Text("李四"), Fx::Text("财务部"), Fx::Num(200.0)],
                ],
            )],
        );
        let new = fixture(
            "cells-new",
            &[(
                "台账",
                vec![
                    vec![Fx::Text("姓名"), Fx::Text("部门"), Fx::Text("金额")],
                    vec![Fx::Text("张三"), Fx::Text("综合部"), Fx::Num(150.0)],
                    vec![Fx::Text("王五"), Fx::Text("营销部"), Fx::Num(300.0)],
                ],
            )],
        );
        let mut config = Config::default();
        config.xlsx.key_columns = vec![1];
        let output = run(&old.path, &new.path, &config);

        assert_eq!(output.old_sheet_count, 1);
        assert_eq!(output.summary.rows_added, 1);
        assert_eq!(output.summary.rows_removed, 1);
        assert_eq!(output.summary.cells_modified, 1);
        assert_eq!(output.summary.columns_modified, 1);
        assert!(output.has_differences());

        let sheet = &output.sheets[0];
        let modified = sheet
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .find(|cell| cell.new_address.as_deref() == Some("C2"))
            .unwrap();
        assert_eq!(modified.status, Status::Modified);
        assert!(modified.value_changed);
        assert_eq!(
            modified.new_value.as_ref().map(CellValue::display),
            Some("150".to_string())
        );
        assert!(sheet.rows.iter().any(|row| row.status == Status::Removed));
        assert!(sheet.rows.iter().any(|row| row.status == Status::Added));
    }

    #[test]
    fn detects_moved_rows_and_columns() {
        let old = fixture(
            "moved-old",
            &[(
                "S",
                vec![
                    vec![Fx::Text("ID"), Fx::Text("本月"), Fx::Text("累计")],
                    vec![Fx::Text("A"), Fx::Num(1.0), Fx::Num(10.0)],
                    vec![Fx::Text("B"), Fx::Num(2.0), Fx::Num(20.0)],
                    vec![Fx::Text("C"), Fx::Num(3.0), Fx::Num(30.0)],
                ],
            )],
        );
        let new = fixture(
            "moved-new",
            &[(
                "S",
                vec![
                    vec![Fx::Text("ID"), Fx::Text("累计"), Fx::Text("本月")],
                    vec![Fx::Text("C"), Fx::Num(30.0), Fx::Num(3.0)],
                    vec![Fx::Text("A"), Fx::Num(10.0), Fx::Num(1.0)],
                    vec![Fx::Text("B"), Fx::Num(20.0), Fx::Num(2.0)],
                ],
            )],
        );
        let output = run(&old.path, &new.path, &Config::default());

        assert_eq!(output.summary.columns_moved, 1);
        assert_eq!(output.summary.rows_moved, 1);
        assert!(
            output.sheets[0]
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .any(|cell| cell.status == Status::Moved)
        );
    }

    #[test]
    fn compare_mode_controls_value_and_formula_differences() {
        let old = fixture(
            "formula-old",
            &[(
                "S",
                vec![
                    vec![Fx::Text("名称"), Fx::Text("金额")],
                    vec![Fx::Text("A"), Fx::Formula("B2*2", 2.0)],
                ],
            )],
        );
        let new = fixture(
            "formula-new",
            &[(
                "S",
                vec![
                    vec![Fx::Text("名称"), Fx::Text("金额")],
                    vec![Fx::Text("A"), Fx::Formula("B2+B2", 2.0)],
                ],
            )],
        );

        let value_only = run(&old.path, &new.path, &Config::default());
        assert!(!value_only.has_differences());

        let mut formula_config = Config::default();
        formula_config.xlsx.compare_mode = XlsxCompareMode::Formula;
        let formula_only = run(&old.path, &new.path, &formula_config);
        assert_eq!(formula_only.summary.cells_modified, 1);
        let cell = formula_only.sheets[0]
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .find(|cell| cell.new_address.as_deref() == Some("B2"))
            .unwrap();
        assert!(cell.formula_changed);
        assert!(!cell.value_changed);
        assert_eq!(cell.old_formula.as_deref(), Some("B2*2"));
        assert_eq!(cell.new_formula.as_deref(), Some("B2+B2"));
    }

    #[test]
    fn auto_detects_title_and_header_and_reports_header_rename() {
        let old = fixture_with_merges(
            "title-old",
            &[(
                "台账",
                vec![
                    vec![Fx::Text("2025年人员台账")],
                    vec![Fx::Text("工号"), Fx::Text("姓名"), Fx::Text("部门")],
                    vec![Fx::Text("EMP001"), Fx::Text("张伟"), Fx::Text("综合部")],
                    vec![Fx::Text("EMP002"), Fx::Text("王芳"), Fx::Text("财务部")],
                ],
            )],
            &[vec!["A1:C1"]],
        );
        let new = fixture_with_merges(
            "title-new",
            &[(
                "台账",
                vec![
                    vec![Fx::Text("2026年人员台账")],
                    vec![Fx::Text("工号"), Fx::Text("姓名"), Fx::Text("所属部门")],
                    vec![Fx::Text("EMP001"), Fx::Text("张伟"), Fx::Text("综合部")],
                    vec![Fx::Text("EMP002"), Fx::Text("王芳"), Fx::Text("财务部")],
                ],
            )],
            &[vec!["A1:C1"]],
        );
        let output = run(&old.path, &new.path, &Config::default());
        let sheet = &output.sheets[0];

        assert_eq!(sheet.header_row, Some(2));
        assert_eq!(output.summary.columns_modified, 1);
        let column = sheet
            .columns
            .iter()
            .find(|column| column.label == "所属部门")
            .unwrap();
        assert_eq!(column.status, Status::Modified);

        let header = sheet
            .rows
            .iter()
            .find(|row| row.new_row == Some(2))
            .unwrap();
        assert_eq!(header.old_row, Some(2));
        assert_eq!(header.status, Status::Modified);
        assert_eq!(header.cells[2].old_address.as_deref(), Some("C2"));
        assert_eq!(header.cells[2].new_address.as_deref(), Some("C2"));
        assert_eq!(header.cells[2].status, Status::Modified);

        // 标题行仍参与比较，且数据行没有被表头识别吞掉。
        assert!(
            sheet
                .rows
                .iter()
                .any(|row| row.old_row == Some(1) && row.status == Status::Modified)
        );
        assert!(sheet.rows.iter().any(|row| row.old_row == Some(3)));
    }

    #[test]
    fn compares_headers_after_a_title_row_is_added() {
        let old = fixture(
            "shifted-header-old",
            &[(
                "台账",
                vec![
                    vec![Fx::Text("工号"), Fx::Text("姓名")],
                    vec![Fx::Text("EMP001"), Fx::Text("张伟")],
                    vec![Fx::Text("EMP002"), Fx::Text("王芳")],
                ],
            )],
        );
        let new = fixture_with_merges(
            "shifted-header-new",
            &[(
                "台账",
                vec![
                    vec![Fx::Text("2026年人员台账")],
                    vec![Fx::Text("工号"), Fx::Text("姓名")],
                    vec![Fx::Text("EMP001"), Fx::Text("张伟")],
                    vec![Fx::Text("EMP002"), Fx::Text("王芳")],
                ],
            )],
            &[vec!["A1:B1"]],
        );
        let output = run(&old.path, &new.path, &Config::default());
        let sheet = &output.sheets[0];

        assert_eq!(sheet.header_row, Some(2));
        let header = sheet
            .rows
            .iter()
            .find(|row| row.new_row == Some(2))
            .unwrap();
        assert_eq!(header.old_row, Some(1));
        assert_eq!(header.status, Status::Unchanged);
        assert!(
            header
                .cells
                .iter()
                .all(|cell| cell.status == Status::Unchanged)
        );

        let data = sheet
            .rows
            .iter()
            .find(|row| row.new_row == Some(3))
            .unwrap();
        assert_eq!(data.old_row, Some(2));
        assert_eq!(data.status, Status::Unchanged);
        assert_eq!(output.summary.rows_modified, 0);
        assert_eq!(output.summary.rows_added, 1);
        assert_eq!(output.summary.rows_removed, 0);
    }

    #[test]
    fn integer_and_float_values_compare_as_numbers() {
        let integer = CellValue::Integer(100);
        let float = CellValue::Float(100.0);
        assert!(values_equal(Some(&integer), Some(&float)));
        assert!(types_equal(&integer, &float));
        assert!(!values_equal(
            Some(&integer),
            Some(&CellValue::Text("100".to_string()))
        ));
        assert!(!types_equal(&integer, &CellValue::Text("100".to_string())));
        assert!(!values_equal(
            Some(&CellValue::Float(100.0)),
            Some(&CellValue::Float(100.5))
        ));
    }

    #[test]
    fn one_sided_sheets_are_reported_and_summarized() {
        let old = fixture(
            "sheets-old",
            &[("保留", vec![vec![Fx::Text("ID")], vec![Fx::Num(1.0)]])],
        );
        let new = fixture(
            "sheets-new",
            &[
                ("保留", vec![vec![Fx::Text("ID")], vec![Fx::Num(1.0)]]),
                ("新增", vec![vec![Fx::Text("ID")], vec![Fx::Num(2.0)]]),
            ],
        );
        let output = run(&old.path, &new.path, &Config::default());

        assert_eq!(output.summary.sheets_added, 1);
        assert_eq!(output.summary.rows_added, 2);
        assert_eq!(output.summary.cells_added, 2);
        let added = output
            .sheets
            .iter()
            .find(|sheet| sheet.name == "新增")
            .unwrap();
        assert_eq!(added.status, Status::Added);
        assert_eq!(added.rows.len(), 2);

        let old = fixture(
            "sheet-removed-old",
            &[
                ("保留", vec![vec![Fx::Text("ID")]]),
                ("移除", vec![vec![Fx::Text("ID")]]),
            ],
        );
        let new = fixture("sheet-removed-new", &[("保留", vec![vec![Fx::Text("ID")]])]);
        let output = run(&old.path, &new.path, &Config::default());
        assert_eq!(output.summary.sheets_removed, 1);
    }
}
