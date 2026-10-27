use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

/// 段落状态。序列化为 snake_case，与需求文档中的输出字段一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Unchanged,
    Added,
    Removed,
    Modified,
    Moved,
    SuspectedModified,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Unchanged => "unchanged",
            Status::Added => "added",
            Status::Removed => "removed",
            Status::Modified => "modified",
            Status::Moved => "moved",
            Status::SuspectedModified => "suspected_modified",
        }
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            Status::Unchanged => "未变",
            Status::Added => "新增",
            Status::Removed => "删除",
            Status::Modified => "修改",
            Status::Moved => "移动",
            Status::SuspectedModified => "疑似修改",
        }
    }

    pub fn is_modified_like(self) -> bool {
        matches!(self, Status::Modified | Status::SuspectedModified)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Scores {
    pub text: f64,
    pub semantic: Option<f64>,
    pub total: f64,
}

impl Scores {
    pub fn exact(semantic_available: bool) -> Self {
        Self {
            text: 1.0,
            semantic: semantic_available.then_some(1.0),
            total: 1.0,
        }
    }

    pub fn empty() -> Self {
        Self {
            text: 0.0,
            semantic: None,
            total: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Equal,
    Insert,
    Delete,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffSpan {
    pub op: DiffOp,
    pub text: String,
}

/// 同一处修改的旧侧/新侧视图。旧侧含 Equal+Delete，新侧含 Equal+Insert。
#[derive(Debug, Clone, Serialize)]
pub struct ParaDiff {
    pub old: Vec<DiffSpan>,
    pub new: Vec<DiffSpan>,
}

/// 表格一行中单个单元格的变化。仅变化单元格会进入记录。
#[derive(Debug, Clone, Serialize)]
pub struct CellChange {
    /// 对齐后的列名，优先取新表表头。
    pub column: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<ParaDiff>,
}

/// 表格记录的位置。表号、行号对外统一使用 1 起始编号。
#[derive(Debug, Clone, Serialize)]
pub struct TableLocation {
    pub table: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_row: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_row: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

impl TableLocation {
    pub fn label(&self, old: bool) -> String {
        let row = if old { self.old_row } else { self.new_row };
        match row {
            Some(row) => format!("表{}·{}", self.table, row),
            None => format!("表{}", self.table),
        }
    }
}

/// 一条比较记录。段落序号对外统一使用 1 起始编号。
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub id: usize,
    pub status: Status,
    pub old_para_index: Option<usize>,
    pub new_para_index: Option<usize>,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub scores: Scores,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<ParaDiff>,
    /// 表格行专用：只列出内容发生变化的单元格。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_diff: Option<Vec<CellChange>>,
    pub matched_to: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<TableLocation>,
    #[serde(skip)]
    pub order: f64,
}

impl Record {
    pub fn display_text(&self) -> &str {
        self.new_text
            .as_deref()
            .or(self.old_text.as_deref())
            .unwrap_or("")
    }

    pub fn position_label(&self, old: bool) -> String {
        if let Some(table) = &self.table {
            return table.label(old);
        }
        let index = if old {
            self.old_para_index
        } else {
            self.new_para_index
        };
        match index {
            Some(index) => format!("#{index}"),
            None => "—".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub unchanged: usize,
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
    pub moved: usize,
    pub suspected_modified: usize,
}

impl Summary {
    pub fn from_records(records: &[Record]) -> Self {
        let mut summary = Self::default();
        for record in records {
            match record.status {
                Status::Unchanged => summary.unchanged += 1,
                Status::Added => summary.added += 1,
                Status::Removed => summary.removed += 1,
                Status::Modified => summary.modified += 1,
                Status::Moved => summary.moved += 1,
                Status::SuspectedModified => summary.suspected_modified += 1,
            }
        }
        summary
    }

    pub fn total(&self) -> usize {
        self.unchanged
            + self.added
            + self.removed
            + self.modified
            + self.moved
            + self.suspected_modified
    }

    pub fn differences(&self) -> usize {
        self.total() - self.unchanged
    }

    pub fn has_differences(&self) -> bool {
        self.differences() > 0
    }

    pub fn brief(&self) -> String {
        format!(
            "未变 {} | 修改 {} | 疑似 {} | 新增 {} | 删除 {} | 移动 {}",
            self.unchanged,
            self.modified,
            self.suspected_modified,
            self.added,
            self.removed,
            self.moved
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridColumnKind {
    Unchanged,
    Added,
    Removed,
}

#[derive(Debug, Clone)]
pub struct GridColumn {
    pub label: String,
    /// 旧表同名/同位置列的原始表头，用于显示列改名。
    pub old_label: Option<String>,
    pub old_index: Option<usize>,
    pub new_index: Option<usize>,
    pub kind: GridColumnKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridCellKind {
    Unchanged,
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone)]
pub struct GridCell {
    pub old: Option<String>,
    pub new: Option<String>,
    pub diff: Option<ParaDiff>,
    pub kind: GridCellKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridRowKind {
    Unchanged,
    Added,
    Removed,
    Modified,
    SuspectedModified,
    MovedFrom,
    MovedTo,
}

#[derive(Debug, Clone)]
pub struct GridRow {
    pub kind: GridRowKind,
    pub old_row: Option<usize>,
    pub new_row: Option<usize>,
    pub cells: Vec<GridCell>,
    pub note: Option<String>,
}

/// Word 表格的整表对齐结果，供修订视图按真实表格渲染。
#[derive(Debug, Clone)]
pub struct TableGrid {
    pub table: usize,
    pub caption: Option<String>,
    pub order: f64,
    pub columns: Vec<GridColumn>,
    pub rows: Vec<GridRow>,
    pub summary: Summary,
    /// 整表未配对时记录新增/删除；此时 columns/rows 来自单侧表格。
    pub whole: Option<Status>,
}

#[derive(Debug, Clone, Default)]
pub struct SemanticStatus {
    pub enabled: bool,
    /// 语义匹配未启用时的原因。
    pub reason: Option<String>,
    pub model_path: Option<String>,
    pub dim: Option<usize>,
}

impl SemanticStatus {
    pub fn enabled(model_path: Option<String>, dim: Option<usize>) -> Self {
        Self {
            enabled: true,
            reason: None,
            model_path,
            dim,
        }
    }

    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            reason: Some(reason.into()),
            model_path: None,
            dim: None,
        }
    }

    pub fn short_label(&self) -> String {
        if self.enabled {
            match (&self.model_path, self.dim) {
                (Some(path), Some(dim)) => {
                    let name = std::path::Path::new(path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(path);
                    format!("语义匹配已启用（{name}，{dim} 维）")
                }
                _ => "语义匹配已启用".to_string(),
            }
        } else {
            format!(
                "语义匹配未启用：{}",
                self.reason.as_deref().unwrap_or("未配置")
            )
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompareOutput {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub old_paragraphs: usize,
    pub new_paragraphs: usize,
    pub old_tables: usize,
    pub new_tables: usize,
    pub records: Vec<Record>,
    pub tables: Vec<TableGrid>,
    pub summary: Summary,
    pub semantic: SemanticStatus,
    pub elapsed: Duration,
}

impl CompareOutput {
    pub fn file_label(path: &std::path::Path) -> String {
        path.file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| path.display().to_string())
    }
}
