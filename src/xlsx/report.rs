use crate::result::Status;

use super::result::XlsxOutput;

pub fn check_report(output: &XlsxOutput) -> String {
    let summary = &output.summary;
    let mut text = format!(
        "旧文件：{}（{} 个工作表）\n新文件：{}（{} 个工作表）\n",
        output.old_path.display(),
        output.old_sheet_count,
        output.new_path.display(),
        output.new_sheet_count
    );
    text.push_str(&format!(
        "结果：工作表 修改 {} 新增 {} 删除 {} | 行 修改 {} 新增 {} 删除 {} 移动 {} | 列 修改 {} 新增 {} 删除 {} 移动 {} | 单元格 修改 {} 新增 {} 删除 {} 移动 {}\n",
        summary.sheets_modified,
        summary.sheets_added,
        summary.sheets_removed,
        summary.rows_modified,
        summary.rows_added,
        summary.rows_removed,
        summary.rows_moved,
        summary.columns_modified,
        summary.columns_added,
        summary.columns_removed,
        summary.columns_moved,
        summary.cells_modified,
        summary.cells_added,
        summary.cells_removed,
        summary.cells_moved
    ));
    for sheet in &output.sheets {
        let header = sheet
            .header_row
            .map(|row| format!("表头第 {row} 行"))
            .unwrap_or_else(|| "无表头".to_string());
        text.push_str(&format!(
            "  {}：{}（{}，单元格差异 {}）\n",
            sheet.name,
            status_label(sheet.status),
            header,
            sheet
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .filter(|cell| !matches!(cell.status, Status::Unchanged))
                .count()
        ));
    }
    if let Some(note) = &output.note {
        text.push_str(&format!("说明：{note}\n"));
    }
    text.push_str(&format!(
        "结论：{}\n耗时：{:.2}s\n",
        if output.has_differences() {
            "存在差异"
        } else {
            "未发现差异"
        },
        output.elapsed.as_secs_f64()
    ));
    text
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Unchanged => "未变",
        Status::Modified => "修改",
        Status::Added => "新增",
        Status::Removed => "删除",
        Status::Moved => "移动",
        Status::SuspectedModified => "疑似修改",
    }
}
