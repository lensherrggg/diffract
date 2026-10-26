use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use crate::result::{ParaDiff, Status};

use super::model::CellValue;

#[derive(Debug, Clone, Serialize)]
pub struct XlsxOutput {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub old_sheet_count: usize,
    pub new_sheet_count: usize,
    pub sheets: Vec<SheetDiff>,
    pub summary: XlsxSummary,
    pub elapsed: Duration,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl XlsxOutput {
    pub fn has_differences(&self) -> bool {
        self.summary.has_differences()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct XlsxSummary {
    pub sheets_added: usize,
    pub sheets_removed: usize,
    pub sheets_modified: usize,
    pub rows_added: usize,
    pub rows_removed: usize,
    pub rows_moved: usize,
    pub rows_modified: usize,
    pub columns_added: usize,
    pub columns_removed: usize,
    pub columns_moved: usize,
    pub columns_modified: usize,
    pub cells_modified: usize,
    pub cells_added: usize,
    pub cells_removed: usize,
    pub cells_moved: usize,
    pub cells_unchanged: usize,
}

impl XlsxSummary {
    pub fn has_differences(&self) -> bool {
        self.sheets_added
            + self.sheets_removed
            + self.sheets_modified
            + self.rows_added
            + self.rows_removed
            + self.rows_moved
            + self.columns_added
            + self.columns_removed
            + self.columns_moved
            + self.cells_modified
            + self.cells_added
            + self.cells_removed
            + self.cells_moved
            > 0
    }

    pub fn cell_differences(&self) -> usize {
        self.cells_modified + self.cells_added + self.cells_removed + self.cells_moved
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SheetDiff {
    pub name: String,
    pub status: Status,
    /// 1 基表头行；`None` 表示无表头。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_row: Option<usize>,
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<GridRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    /// 表头文字，缺省时使用 Excel 列标。
    pub label: String,
    pub old_col: Option<usize>,
    pub new_col: Option<usize>,
    pub status: Status,
}

#[derive(Debug, Clone, Serialize)]
pub struct GridRow {
    pub old_row: Option<usize>,
    pub new_row: Option<usize>,
    pub status: Status,
    pub cells: Vec<GridCell>,
}

impl GridRow {
    pub fn number(&self) -> usize {
        self.new_row.or(self.old_row).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GridCell {
    pub status: Status,
    pub old_col: Option<usize>,
    pub new_col: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_value: Option<CellValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_value: Option<CellValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_formula: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_formula: Option<String>,
    pub value_changed: bool,
    pub formula_changed: bool,
    pub type_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<ParaDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl GridCell {
    pub fn display(&self) -> String {
        match self.status {
            Status::Removed => self
                .old_value
                .as_ref()
                .map(CellValue::display)
                .unwrap_or_default(),
            _ => self
                .new_value
                .as_ref()
                .map(CellValue::display)
                .or_else(|| self.old_value.as_ref().map(CellValue::display))
                .unwrap_or_default(),
        }
    }
}
