use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, bail};
use calamine::{Data, ExcelDateTime, Reader, Xlsx, open_workbook};

use super::model::{Cell, CellValue, Sheet, SheetRow, Workbook, detect_header_row};

pub fn read_workbook(path: &Path) -> Result<Workbook> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "xls" {
        bail!(
            "暂不支持 .xls：{}，请先在 Excel/WPS 中另存为 .xlsx",
            path.display()
        );
    }
    if extension != "xlsx" && extension != "xlsm" {
        bail!("Excel 引擎只支持 .xlsx/.xlsm：{}", path.display());
    }

    let mut workbook: Xlsx<_> =
        open_workbook(path).with_context(|| format!("打开 Excel 文件失败：{}", path.display()))?;
    let names = workbook.sheet_names();
    if names.is_empty() {
        bail!("Excel 文件没有工作表：{}", path.display());
    }

    let mut sheets = Vec::with_capacity(names.len());
    for name in names {
        let range = workbook
            .worksheet_range(&name)
            .with_context(|| format!("读取工作表失败：{name}"))?;
        let formulas = workbook.worksheet_formula(&name).ok();
        let formula_start = formulas
            .as_ref()
            .and_then(|range| range.start())
            .unwrap_or((0, 0));
        let mut formula_map: HashMap<(usize, usize), String> = HashMap::new();
        if let Some(formulas) = &formulas {
            for row in 0..formulas.height() {
                for column in 0..formulas.width() {
                    if let Some(formula) = formulas.get((row, column))
                        && !formula.trim().is_empty()
                    {
                        formula_map.insert(
                            (
                                formula_start.0 as usize + row,
                                formula_start.1 as usize + column,
                            ),
                            formula.trim().to_string(),
                        );
                    }
                }
            }
        }

        let start = range.start().unwrap_or((0, 0));
        let mut rows: Vec<SheetRow> = Vec::new();
        for row_index in 0..range.height() {
            let mut cells = BTreeMap::new();
            for column_index in 0..range.width() {
                let absolute_row = start.0 as usize + row_index;
                let absolute_column = start.1 as usize + column_index;
                let value = range
                    .get((row_index, column_index))
                    .map(cell_value)
                    .unwrap_or(CellValue::Empty);
                let formula = formula_map.get(&(absolute_row, absolute_column)).cloned();
                let cell = Cell { value, formula };
                if !cell.is_empty() {
                    cells.insert(absolute_column + 1, cell);
                }
            }
            if !cells.is_empty() {
                rows.push(SheetRow {
                    number: start.0 as usize + row_index + 1,
                    cells,
                });
            }
        }
        rows.sort_by_key(|row| row.number);
        let wide_merged_rows = match workbook.merge_cells_by_sheet_name(&name) {
            Ok(regions) => regions
                .into_iter()
                .filter(|region| region.end.1 > region.start.1)
                .flat_map(|region| (region.start.0..=region.end.0).map(|row| row as usize + 1))
                .collect::<Vec<_>>(),
            Err(error) => {
                log::warn!("读取合并区域失败：{name}：{error}");
                Vec::new()
            }
        };
        let header_row = detect_header_row(&rows, &wide_merged_rows);
        sheets.push(Sheet {
            name,
            rows,
            header_row,
        });
    }

    Ok(Workbook {
        path: path.to_path_buf(),
        sheets,
    })
}

fn cell_value(data: &Data) -> CellValue {
    match data {
        Data::Int(value) => CellValue::Integer(*value),
        Data::Float(value) => CellValue::Float(*value),
        Data::String(value) => CellValue::Text(value.clone()),
        Data::Bool(value) => CellValue::Bool(*value),
        Data::DateTime(value) => CellValue::DateTime(format_datetime(value)),
        Data::DateTimeIso(value) => CellValue::DateTime(value.clone()),
        Data::DurationIso(value) => CellValue::Text(value.clone()),
        Data::Error(value) => CellValue::Error(format!("{value:?}")),
        Data::Empty => CellValue::Empty,
    }
}

fn format_datetime(value: &ExcelDateTime) -> String {
    let (year, month, day, hour, minute, second, milli) = value.to_ymd_hms_milli();
    if hour == 0 && minute == 0 && second == 0 && milli == 0 {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
    }
}
