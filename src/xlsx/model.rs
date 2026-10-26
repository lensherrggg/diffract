use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellValue {
    #[default]
    Empty,
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    DateTime(String),
    Error(String),
}

impl CellValue {
    pub fn is_empty(&self) -> bool {
        matches!(self, CellValue::Empty)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            CellValue::Empty => "empty",
            CellValue::Text(_) => "text",
            CellValue::Integer(_) => "integer",
            CellValue::Float(_) => "float",
            CellValue::Bool(_) => "bool",
            CellValue::DateTime(_) => "datetime",
            CellValue::Error(_) => "error",
        }
    }

    pub fn display(&self) -> String {
        match self {
            CellValue::Empty => String::new(),
            CellValue::Text(value) => value.clone(),
            CellValue::Integer(value) => value.to_string(),
            CellValue::Float(value) => {
                let mut text = format!("{value}");
                if text.contains('.') {
                    while text.ends_with('0') {
                        text.pop();
                    }
                    if text.ends_with('.') {
                        text.pop();
                    }
                }
                text
            }
            CellValue::Bool(value) => {
                if *value {
                    "TRUE".to_string()
                } else {
                    "FALSE".to_string()
                }
            }
            CellValue::DateTime(value) => value.clone(),
            CellValue::Error(value) => value.clone(),
        }
    }

    pub fn same_type(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Cell {
    pub value: CellValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formula: Option<String>,
}

impl Cell {
    pub fn is_empty(&self) -> bool {
        self.value.is_empty() && self.formula.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SheetRow {
    /// 1 基行号。
    pub number: usize,
    /// 1 基列号 → 单元格。
    pub cells: BTreeMap<usize, Cell>,
}

impl SheetRow {
    pub fn cell(&self, column: usize) -> Option<&Cell> {
        self.cells.get(&column)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<SheetRow>,
    /// 自动识别的 1 基表头行；`None` 表示按无表头处理。
    pub header_row: Option<usize>,
}

impl Sheet {
    pub fn row(&self, number: usize) -> Option<&SheetRow> {
        self.rows.iter().find(|row| row.number == number)
    }

    pub fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        self.row(row).and_then(|row| row.cell(column))
    }

    pub fn used_columns(&self) -> Vec<usize> {
        let mut columns: Vec<usize> = self
            .rows
            .iter()
            .flat_map(|row| row.cells.keys().copied())
            .collect();
        columns.sort_unstable();
        columns.dedup();
        columns
    }
}

/// 识别表头行：跳过跨列合并的标题行，取第一行有至少两个文本单元格的数据行。
/// 找不到时退回第一条非空行；单列/无表头文件可通过配置关闭识别。
pub fn detect_header_row(rows: &[SheetRow], wide_merged_rows: &[usize]) -> Option<usize> {
    let fallback = rows.first().map(|row| row.number);
    for row in rows.iter().take(6) {
        if wide_merged_rows.contains(&row.number) {
            continue;
        }
        let mut non_empty = 0;
        let mut text = 0;
        for cell in row.cells.values().filter(|cell| !cell.is_empty()) {
            non_empty += 1;
            if matches!(cell.value, CellValue::Text(_)) {
                text += 1;
            }
        }
        if non_empty >= 2 && text >= 2 {
            return Some(row.number);
        }
    }
    fallback
}

#[derive(Debug, Clone, Serialize)]
pub struct Workbook {
    pub path: PathBuf,
    pub sheets: Vec<Sheet>,
}

pub fn column_label(column: usize) -> String {
    if column == 0 {
        return "?".to_string();
    }
    let mut value = column;
    let mut label = String::new();
    while value > 0 {
        let remainder = (value - 1) % 26;
        label.insert(0, (b'A' + remainder as u8) as char);
        value = (value - 1) / 26;
    }
    label
}

pub fn address(row: usize, column: usize) -> String {
    format!("{}{}", column_label(column), row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_excel_column_labels() {
        assert_eq!(column_label(1), "A");
        assert_eq!(column_label(26), "Z");
        assert_eq!(column_label(27), "AA");
        assert_eq!(column_label(52), "AZ");
        assert_eq!(column_label(53), "BA");
    }

    #[test]
    fn formats_values_for_display() {
        assert_eq!(CellValue::Float(1.5).display(), "1.5");
        assert_eq!(CellValue::Float(2.0).display(), "2");
        assert_eq!(CellValue::Bool(true).display(), "TRUE");
        assert_eq!(CellValue::Text("abc".into()).display(), "abc");
    }

    fn row(number: usize, values: &[(usize, &str)]) -> SheetRow {
        SheetRow {
            number,
            cells: values
                .iter()
                .map(|(column, value)| {
                    (
                        *column,
                        Cell {
                            value: CellValue::Text((*value).to_string()),
                            formula: None,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn detects_header_after_a_merged_title_row() {
        let rows = vec![
            row(1, &[(1, "2026年人员台账")]),
            row(2, &[(1, "工号"), (2, "姓名"), (3, "部门")]),
            row(3, &[(1, "EMP001"), (2, "张伟"), (3, "综合部")]),
        ];
        assert_eq!(detect_header_row(&rows, &[1]), Some(2));
    }

    #[test]
    fn detects_header_after_a_single_cell_title() {
        let rows = vec![
            row(1, &[(1, "预算执行情况")]),
            row(2, &[(1, "项目"), (2, "预算"), (3, "实际")]),
        ];
        assert_eq!(detect_header_row(&rows, &[]), Some(2));
    }

    #[test]
    fn falls_back_to_the_first_row_without_a_clear_header() {
        let rows = vec![row(1, &[(1, "仅一列")]), row(2, &[(1, "数据")])];
        assert_eq!(detect_header_row(&rows, &[]), Some(1));
        assert_eq!(detect_header_row(&[], &[]), None);
    }
}
