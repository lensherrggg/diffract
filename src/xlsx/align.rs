use std::collections::HashMap;

use crate::config::{XlsxCompareMode, XlsxConfig};

use super::model::{Sheet, SheetRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnPair {
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub moved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowPair {
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub moved: bool,
}

#[derive(Debug, Clone)]
pub struct Alignment {
    pub columns: Vec<ColumnPair>,
    pub rows: Vec<RowPair>,
}

pub fn align_sheets(
    old: &Sheet,
    new: &Sheet,
    config: &XlsxConfig,
    old_header_row: Option<usize>,
    new_header_row: Option<usize>,
) -> Alignment {
    let columns = align_columns(old, new, old_header_row, new_header_row);
    let rows = align_rows(old, new, &columns, config, old_header_row, new_header_row);
    Alignment { columns, rows }
}

fn align_columns(
    old: &Sheet,
    new: &Sheet,
    old_header_row: Option<usize>,
    new_header_row: Option<usize>,
) -> Vec<ColumnPair> {
    let old_columns = old.used_columns();
    let new_columns = new.used_columns();
    let mut used_old = vec![false; old_columns.len()];
    let mut used_new = vec![false; new_columns.len()];
    let mut pairs: Vec<ColumnPair> = Vec::new();

    if old_header_row.is_some() || new_header_row.is_some() {
        let old_headers = header_map(old, old_header_row, &old_columns);
        let new_headers = header_map(new, new_header_row, &new_columns);
        for (label, old_indexes) in &old_headers {
            if old_indexes.len() != 1 {
                continue;
            }
            let Some(new_indexes) = new_headers.get(label) else {
                continue;
            };
            if new_indexes.len() != 1 {
                continue;
            }
            let old_pos = old_indexes[0];
            let new_pos = new_indexes[0];
            if used_old[old_pos] || used_new[new_pos] {
                continue;
            }
            used_old[old_pos] = true;
            used_new[new_pos] = true;
            pairs.push(ColumnPair {
                old: Some(old_columns[old_pos]),
                new: Some(new_columns[new_pos]),
                moved: false,
            });
        }
    }

    let old_signatures = column_signatures(old, &old_columns, old_header_row);
    let new_signatures = column_signatures(new, &new_columns, new_header_row);
    let mut new_by_signature: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, signature) in &new_signatures {
        if !signature.is_empty() && !used_new[*index] {
            new_by_signature
                .entry(signature.as_str())
                .or_default()
                .push(*index);
        }
    }
    for (index, signature) in &old_signatures {
        if signature.is_empty() || used_old[*index] {
            continue;
        }
        let Some(candidates) = new_by_signature.get_mut(signature.as_str()) else {
            continue;
        };
        let Some(new_index) = candidates
            .iter()
            .position(|candidate| !used_new[*candidate])
        else {
            continue;
        };
        let new_index = candidates.remove(new_index);
        used_old[*index] = true;
        used_new[new_index] = true;
        pairs.push(ColumnPair {
            old: Some(old_columns[*index]),
            new: Some(new_columns[new_index]),
            moved: false,
        });
    }

    let remaining_old: Vec<usize> = old_columns
        .iter()
        .enumerate()
        .filter(|(index, _)| !used_old[*index])
        .map(|(_, column)| *column)
        .collect();
    let remaining_new: Vec<usize> = new_columns
        .iter()
        .enumerate()
        .filter(|(index, _)| !used_new[*index])
        .map(|(_, column)| *column)
        .collect();
    for (old_column, new_column) in remaining_old.iter().zip(remaining_new.iter()) {
        pairs.push(ColumnPair {
            old: Some(*old_column),
            new: Some(*new_column),
            moved: false,
        });
    }
    for old_column in remaining_old.iter().skip(remaining_new.len()) {
        pairs.push(ColumnPair {
            old: Some(*old_column),
            new: None,
            moved: false,
        });
    }
    for new_column in remaining_new.iter().skip(remaining_old.len()) {
        pairs.push(ColumnPair {
            old: None,
            new: Some(*new_column),
            moved: false,
        });
    }

    mark_column_moves(&mut pairs);
    order_columns(pairs)
}

fn header_map(
    sheet: &Sheet,
    header_row: Option<usize>,
    columns: &[usize],
) -> HashMap<String, Vec<usize>> {
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();
    let Some(header_row) = header_row else {
        return map;
    };
    for (index, column) in columns.iter().enumerate() {
        let Some(cell) = sheet.cell(header_row, *column) else {
            continue;
        };
        let label = cell.value.display().trim().to_string();
        if label.is_empty() {
            continue;
        }
        map.entry(label).or_default().push(index);
    }
    map
}

fn column_signatures(
    sheet: &Sheet,
    columns: &[usize],
    header_row: Option<usize>,
) -> Vec<(usize, String)> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let values: Vec<String> = sheet
                .rows
                .iter()
                .filter(|row| Some(row.number) != header_row)
                .filter_map(|row| row.cell(*column))
                .filter(|cell| !cell.is_empty())
                .map(|cell| cell.value.display())
                .collect();
            (index, values.join("\u{1f}"))
        })
        .collect()
}

fn mark_column_moves(pairs: &mut [ColumnPair]) {
    let mut paired: Vec<(usize, usize)> = pairs
        .iter()
        .filter_map(|pair| pair.old.zip(pair.new))
        .collect();
    paired.sort_unstable();
    let new_order: Vec<usize> = paired.iter().map(|(_, new)| *new).collect();
    let members = lis_members(&new_order);
    for (index, (old, new)) in paired.iter().enumerate() {
        if members[index] {
            continue;
        }
        if let Some(pair) = pairs
            .iter_mut()
            .find(|pair| pair.old == Some(*old) && pair.new == Some(*new))
        {
            pair.moved = true;
        }
    }
}

fn order_columns(mut pairs: Vec<ColumnPair>) -> Vec<ColumnPair> {
    pairs.sort_by_key(|pair| pair.new.unwrap_or(usize::MAX));
    let new_only_end = pairs
        .iter()
        .position(|pair| pair.new.is_none())
        .unwrap_or(pairs.len());
    let mut ordered = pairs[..new_only_end].to_vec();
    let mut removed = pairs[new_only_end..].to_vec();
    removed.sort_by_key(|pair| pair.old.unwrap_or(usize::MAX));
    ordered.extend(removed);
    ordered
}

fn align_rows(
    old: &Sheet,
    new: &Sheet,
    columns: &[ColumnPair],
    config: &XlsxConfig,
    old_header_row: Option<usize>,
    new_header_row: Option<usize>,
) -> Vec<RowPair> {
    let mode = config.compare_mode;
    let old_rows: Vec<&SheetRow> = old
        .rows
        .iter()
        .filter(|row| Some(row.number) != old_header_row)
        .collect();
    let new_rows: Vec<&SheetRow> = new
        .rows
        .iter()
        .filter(|row| Some(row.number) != new_header_row)
        .collect();
    let mut used_old = vec![false; old_rows.len()];
    let mut used_new = vec![false; new_rows.len()];
    let mut pairs: Vec<RowPair> = Vec::new();

    if old_header_row.is_some() || new_header_row.is_some() {
        let old_titles: Vec<usize> = (0..old_rows.len())
            .filter(|index| old_header_row.is_some_and(|header| old_rows[*index].number < header))
            .collect();
        let new_titles: Vec<usize> = (0..new_rows.len())
            .filter(|index| new_header_row.is_some_and(|header| new_rows[*index].number < header))
            .collect();
        for (old_index, new_index) in old_titles.iter().zip(new_titles.iter()) {
            used_old[*old_index] = true;
            used_new[*new_index] = true;
            pairs.push(RowPair {
                old: Some(old_rows[*old_index].number),
                new: Some(new_rows[*new_index].number),
                moved: false,
            });
        }
    }

    if !config.key_columns.is_empty() {
        let old_keys: Vec<Option<String>> = old_rows
            .iter()
            .map(|row| row_key(old, row.number, columns, true, &config.key_columns))
            .collect();
        let new_keys: Vec<Option<String>> = new_rows
            .iter()
            .map(|row| row_key(new, row.number, columns, false, &config.key_columns))
            .collect();
        let mut new_by_key: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, key) in new_keys.iter().enumerate() {
            if let Some(key) = key
                && !key.is_empty()
            {
                new_by_key.entry(key.as_str()).or_default().push(index);
            }
        }
        for (old_index, key) in old_keys.iter().enumerate() {
            let Some(key) = key else {
                continue;
            };
            let Some(candidates) = new_by_key.get_mut(key.as_str()) else {
                continue;
            };
            let Some(position) = candidates.iter().position(|index| !used_new[*index]) else {
                continue;
            };
            let new_index = candidates.remove(position);
            used_old[old_index] = true;
            used_new[new_index] = true;
            pairs.push(RowPair {
                old: Some(old_rows[old_index].number),
                new: Some(new_rows[new_index].number),
                moved: false,
            });
        }
    }

    let old_signatures: Vec<String> = old_rows
        .iter()
        .map(|row| row_signature(old, row.number, columns, true, mode))
        .collect();
    let new_signatures: Vec<String> = new_rows
        .iter()
        .map(|row| row_signature(new, row.number, columns, false, mode))
        .collect();
    let mut new_by_signature: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, signature) in new_signatures.iter().enumerate() {
        if !signature.is_empty() && !used_new[index] {
            new_by_signature
                .entry(signature.as_str())
                .or_default()
                .push(index);
        }
    }
    for (old_index, signature) in old_signatures.iter().enumerate() {
        if signature.is_empty() || used_old[old_index] {
            continue;
        }
        let Some(candidates) = new_by_signature.get_mut(signature.as_str()) else {
            continue;
        };
        let Some(position) = candidates.iter().position(|index| !used_new[*index]) else {
            continue;
        };
        let new_index = candidates.remove(position);
        used_old[old_index] = true;
        used_new[new_index] = true;
        pairs.push(RowPair {
            old: Some(old_rows[old_index].number),
            new: Some(new_rows[new_index].number),
            moved: false,
        });
    }

    let mut unmatched_old: Vec<usize> = (0..old_rows.len())
        .filter(|index| !used_old[*index])
        .collect();
    let mut unmatched_new: Vec<usize> = (0..new_rows.len())
        .filter(|index| !used_new[*index])
        .collect();
    for old_index in unmatched_old.drain(..) {
        if unmatched_new.is_empty() {
            break;
        }
        let predicted = if new_rows.is_empty() {
            0
        } else {
            old_index * new_rows.len() / old_rows.len().max(1)
        };
        let low = predicted.saturating_sub(config.row_window);
        let high = (predicted + config.row_window + 1).min(new_rows.len());
        let mut best: Option<(f64, usize, usize)> = None;
        for (position, new_index) in unmatched_new.iter().enumerate() {
            if *new_index < low || *new_index >= high {
                continue;
            }
            let score = row_similarity(
                old,
                old_rows[old_index].number,
                new,
                new_rows[*new_index].number,
                columns,
            );
            if score < 0.4 {
                continue;
            }
            if best.is_none_or(|(best_score, _, _)| score > best_score) {
                best = Some((score, *new_index, position));
            }
        }
        if let Some((_, new_index, position)) = best {
            unmatched_new.remove(position);
            used_old[old_index] = true;
            used_new[new_index] = true;
            pairs.push(RowPair {
                old: Some(old_rows[old_index].number),
                new: Some(new_rows[new_index].number),
                moved: false,
            });
        }
    }

    for old_index in (0..old_rows.len()).filter(|index| !used_old[*index]) {
        pairs.push(RowPair {
            old: Some(old_rows[old_index].number),
            new: None,
            moved: false,
        });
    }
    for new_index in (0..new_rows.len()).filter(|index| !used_new[*index]) {
        pairs.push(RowPair {
            old: None,
            new: Some(new_rows[new_index].number),
            moved: false,
        });
    }

    mark_row_moves(&mut pairs);
    order_rows(pairs)
}

fn row_key(
    sheet: &Sheet,
    row: usize,
    columns: &[ColumnPair],
    old_side: bool,
    keys: &[usize],
) -> Option<String> {
    let mut parts = Vec::new();
    for key in keys {
        let pair = columns.iter().find(|pair| {
            if old_side {
                pair.old == Some(*key)
            } else {
                pair.new == Some(*key)
            }
        })?;
        let column = if old_side { pair.old } else { pair.new }?;
        let Some(cell) = sheet.cell(row, column) else {
            continue;
        };
        if cell.is_empty() {
            continue;
        }
        parts.push(cell.value.display());
    }
    (!parts.is_empty()).then(|| parts.join("\u{1f}"))
}

fn row_signature(
    sheet: &Sheet,
    row: usize,
    columns: &[ColumnPair],
    old_side: bool,
    mode: XlsxCompareMode,
) -> String {
    let mut parts = Vec::new();
    for pair in columns {
        let column = if old_side { pair.old } else { pair.new };
        let Some(column) = column else {
            continue;
        };
        let Some(cell) = sheet.cell(row, column) else {
            continue;
        };
        let text = match mode {
            XlsxCompareMode::Value => cell.value.display(),
            XlsxCompareMode::Formula => cell.formula.clone().unwrap_or_default(),
            XlsxCompareMode::Both => format!(
                "{}\u{1e}{}",
                cell.value.display(),
                cell.formula.clone().unwrap_or_default()
            ),
        };
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.join("\u{1f}")
}

fn row_similarity(
    old: &Sheet,
    old_row: usize,
    new: &Sheet,
    new_row: usize,
    columns: &[ColumnPair],
) -> f64 {
    let mut same = 0usize;
    let mut total = 0usize;
    let mut old_text = String::new();
    let mut new_text = String::new();
    for pair in columns {
        let old_cell = pair
            .old
            .and_then(|column| old.cell(old_row, column))
            .filter(|cell| !cell.is_empty());
        let new_cell = pair
            .new
            .and_then(|column| new.cell(new_row, column))
            .filter(|cell| !cell.is_empty());
        if old_cell.is_none() && new_cell.is_none() {
            continue;
        }
        total += 1;
        let old_value = old_cell
            .map(|cell| cell.value.display())
            .unwrap_or_default();
        let new_value = new_cell
            .map(|cell| cell.value.display())
            .unwrap_or_default();
        if old_value == new_value && !old_value.is_empty() {
            same += 1;
        }
        if !old_value.is_empty() {
            old_text.push_str(&old_value);
            old_text.push(' ');
        }
        if !new_value.is_empty() {
            new_text.push_str(&new_value);
            new_text.push(' ');
        }
    }
    if total == 0 {
        return 0.0;
    }
    if old_text == new_text {
        return 1.0;
    }
    let text_score = if old_text.is_empty() || new_text.is_empty() {
        0.0
    } else {
        crate::matching::text_sim::text_similarity(
            &crate::matching::text_sim::TextFeatures::from_normalized(&old_text),
            &crate::matching::text_sim::TextFeatures::from_normalized(&new_text),
        )
    };
    0.5 * (same as f64 / total as f64) + 0.5 * text_score
}

fn mark_row_moves(pairs: &mut [RowPair]) {
    let mut paired: Vec<(usize, usize)> = pairs
        .iter()
        .filter_map(|pair| pair.old.zip(pair.new))
        .collect();
    paired.sort_unstable();
    let new_order: Vec<usize> = paired.iter().map(|(_, new)| *new).collect();
    let members = lis_members(&new_order);
    for (index, (old, new)) in paired.iter().enumerate() {
        if members[index] {
            continue;
        }
        if let Some(pair) = pairs
            .iter_mut()
            .find(|pair| pair.old == Some(*old) && pair.new == Some(*new))
        {
            pair.moved = true;
        }
    }
}

fn order_rows(mut pairs: Vec<RowPair>) -> Vec<RowPair> {
    pairs.sort_by_key(|pair| pair.new.unwrap_or(usize::MAX));
    let split = pairs
        .iter()
        .position(|pair| pair.new.is_none())
        .unwrap_or(pairs.len());
    let mut ordered = pairs[..split].to_vec();
    let mut removed = pairs[split..].to_vec();
    removed.sort_by_key(|pair| pair.old.unwrap_or(usize::MAX));
    ordered.extend(removed);
    ordered
}

fn lis_members(values: &[usize]) -> Vec<bool> {
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![usize::MAX; values.len()];
    for (index, value) in values.iter().copied().enumerate() {
        let position = tails.partition_point(|tail| values[*tail] < value);
        if position > 0 {
            previous[index] = tails[position - 1];
        }
        if position == tails.len() {
            tails.push(index);
        } else {
            tails[position] = index;
        }
    }
    let mut members = vec![false; values.len()];
    if let Some(&last) = tails.last() {
        let mut current = last;
        loop {
            members[current] = true;
            if previous[current] == usize::MAX {
                break;
            }
            current = previous[current];
        }
    }
    members
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::xlsx::model::{Cell, CellValue, SheetRow};

    fn sheet(name: &str, rows: &[(usize, &[(usize, &str)])]) -> Sheet {
        Sheet {
            name: name.to_string(),
            rows: rows
                .iter()
                .map(|(number, cells)| SheetRow {
                    number: *number,
                    cells: cells
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
                        .collect::<BTreeMap<_, _>>(),
                })
                .collect(),
            header_row: Some(1),
        }
    }

    #[test]
    fn aligns_columns_by_header_and_detects_moves() {
        let old = sheet(
            "S",
            &[
                (1, &[(1, "ID"), (2, "本月"), (3, "累计")]),
                (2, &[(1, "1"), (2, "100"), (3, "200")]),
            ],
        );
        let new = sheet(
            "S",
            &[
                (1, &[(1, "ID"), (2, "累计"), (3, "本月")]),
                (2, &[(1, "1"), (2, "200"), (3, "100")]),
            ],
        );
        let alignment = align_sheets(&old, &new, &XlsxConfig::default(), Some(1), Some(1));
        assert_eq!(alignment.columns.len(), 3);
        let moved = alignment
            .columns
            .iter()
            .find(|pair| pair.old == Some(2))
            .unwrap();
        assert!(moved.moved);
    }

    #[test]
    fn aligns_rows_by_content_and_marks_order_changes() {
        let old = sheet(
            "S",
            &[
                (1, &[(1, "ID"), (2, "值")]),
                (2, &[(1, "A"), (2, "1")]),
                (3, &[(1, "B"), (2, "2")]),
            ],
        );
        let new = sheet(
            "S",
            &[
                (1, &[(1, "ID"), (2, "值")]),
                (2, &[(1, "B"), (2, "2")]),
                (3, &[(1, "A"), (2, "1")]),
            ],
        );
        let alignment = align_sheets(&old, &new, &XlsxConfig::default(), Some(1), Some(1));
        assert_eq!(alignment.rows.len(), 2);
        assert_eq!(alignment.rows.iter().filter(|pair| pair.moved).count(), 1);
    }
}
