use std::collections::{HashMap, HashSet};

use crate::config::{Config, SimilarityConfig};
use crate::docx::{Table, TableRow};
use crate::normalize::normalize_for_match;
use crate::result::{Record, Scores, Status, TableLocation};

use super::text_sim::{TextFeatures, cosine, text_similarity};

/// 两张表至少要这么像才配对，否则按整表新增/删除处理。
const TABLE_MATCH_THRESHOLD: f64 = 0.35;

/// 表格行语义向量，按 [表][行] 索引；空行保留为空向量。
pub struct TableVectors {
    pub old: Vec<Vec<Vec<f32>>>,
    pub new: Vec<Vec<Vec<f32>>>,
}

/// 用于嵌入的行文本：剔除本表的序号列、去掉尾部空格，与行比较口径一致。
pub fn semantic_texts(table: &Table) -> Vec<String> {
    let ignored = ignored_columns(table, table);
    table
        .rows
        .iter()
        .map(|row| visible_text(row, &ignored))
        .collect()
}

pub fn match_tables(
    old: &[Table],
    new: &[Table],
    config: &Config,
    semantic: Option<&TableVectors>,
) -> Vec<Record> {
    let mut records = Vec::new();
    let mut old_used = vec![false; old.len()];
    let mut new_used = vec![false; new.len()];

    for (old_index, new_index) in align_tables(old, new) {
        old_used[old_index] = true;
        new_used[new_index] = true;
        records.extend(compare_table_pair(
            &old[old_index],
            &new[new_index],
            config,
            semantic.map(|vectors| vectors.old[old_index].as_slice()),
            semantic.map(|vectors| vectors.new[new_index].as_slice()),
        ));
    }
    for (index, table) in old.iter().enumerate() {
        if !old_used[index] {
            records.push(whole_table_record(table, false));
        }
    }
    for (index, table) in new.iter().enumerate() {
        if !new_used[index] {
            records.push(whole_table_record(table, true));
        }
    }
    records
}

fn align_tables(old: &[Table], new: &[Table]) -> Vec<(usize, usize)> {
    let (n, m) = (old.len(), new.len());
    if n == 0 || m == 0 {
        return Vec::new();
    }
    let mut dp = vec![vec![0.0f64; m + 1]; n + 1];
    let mut choice = vec![vec![0u8; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            let skip_old = dp[i - 1][j];
            let skip_new = dp[i][j - 1];
            let score = table_pair_score(&old[i - 1], &new[j - 1], n, m);
            let pair = if score >= TABLE_MATCH_THRESHOLD {
                dp[i - 1][j - 1] + score
            } else {
                f64::NEG_INFINITY
            };
            if pair >= skip_old && pair >= skip_new {
                dp[i][j] = pair;
                choice[i][j] = 2;
            } else if skip_old >= skip_new {
                dp[i][j] = skip_old;
                choice[i][j] = 0;
            } else {
                dp[i][j] = skip_new;
                choice[i][j] = 1;
            }
        }
    }

    let (mut i, mut j) = (n, m);
    let mut pairs = Vec::new();
    while i > 0 && j > 0 {
        match choice[i][j] {
            2 => {
                pairs.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            }
            0 => i -= 1,
            _ => j -= 1,
        }
    }
    pairs.reverse();
    pairs
}

fn table_pair_score(old: &Table, new: &Table, old_total: usize, new_total: usize) -> f64 {
    let old_keys = key_set(old);
    let new_keys = key_set(new);
    let old_rows = row_set(old);
    let new_rows = row_set(new);

    let keys =
        (!old_keys.is_empty() && !new_keys.is_empty()).then(|| set_jaccard(&old_keys, &new_keys));
    let texts =
        (!old_rows.is_empty() && !new_rows.is_empty()).then(|| set_jaccard(&old_rows, &new_rows));
    let caption = match (&old.caption, &new.caption) {
        (Some(a), Some(b)) => {
            let a = crate::normalize::normalize_for_match(a);
            let b = crate::normalize::normalize_for_match(b);
            Some(text_similarity(
                &TextFeatures::from_normalized(&a),
                &TextFeatures::from_normalized(&b),
            ))
        }
        _ => None,
    };

    let row_ratio = ratio(old.rows.len(), new.rows.len());
    let column_ratio = ratio(max_columns(old), max_columns(new));
    let position = 1.0
        - ((old.index as f64 + 0.5) / old_total as f64
            - (new.index as f64 + 0.5) / new_total as f64)
            .abs();

    let mut score = 0.0;
    let mut weight = 0.0;
    if let Some(value) = keys {
        score += 0.35 * value;
        weight += 0.35;
    }
    if let Some(value) = texts {
        score += 0.35 * value;
        weight += 0.35;
    }
    if let Some(value) = caption {
        score += 0.05 * value;
        weight += 0.05;
    }
    score += 0.10 * row_ratio + 0.05 * column_ratio + 0.10 * position;
    weight += 0.25;
    if weight == 0.0 { 0.0 } else { score / weight }
}

fn key_set(table: &Table) -> HashSet<&str> {
    if ordinal_column(table, 0) {
        return HashSet::new();
    }
    table
        .rows
        .iter()
        .filter_map(|row| row.key.as_deref())
        .collect()
}

fn row_set(table: &Table) -> HashSet<&str> {
    table
        .rows
        .iter()
        .map(|row| row.normalized.as_str())
        .filter(|text| !text.is_empty())
        .collect()
}

fn set_jaccard(a: &HashSet<&str>, b: &HashSet<&str>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

fn ratio(a: usize, b: usize) -> f64 {
    match a.max(b) {
        0 => 1.0,
        max => a.min(b) as f64 / max as f64,
    }
}

fn max_columns(table: &Table) -> usize {
    table
        .rows
        .iter()
        .map(|row| row.cells.len())
        .max()
        .unwrap_or(0)
}

/// 形如 1、2、3… 的序号列。它是行位置而非身份，增删行后整体重编号，
/// 不能当主键，也不参与文本比较。
fn ordinal_column(table: &Table, column: usize) -> bool {
    let mut numbers = Vec::new();
    for row in &table.rows {
        let Some(value) = row.cells.get(column) else {
            continue;
        };
        if let Ok(number) = value.trim().parse::<i64>() {
            numbers.push(number);
        }
    }
    if numbers.len() < 3 || !matches!(numbers[0], 0 | 1) {
        return false;
    }
    let steps = numbers.len() - 1;
    let consecutive = numbers
        .windows(2)
        .filter(|pair| pair[1].checked_sub(pair[0]) == Some(1))
        .count();
    consecutive * 10 >= steps * 8
}

fn ignored_columns(old: &Table, new: &Table) -> Vec<bool> {
    let columns = max_columns(old).max(max_columns(new));
    (0..columns)
        .map(|column| ordinal_column(old, column) || ordinal_column(new, column))
        .collect()
}

fn column_is_usable(table: &Table, column: usize) -> bool {
    let mut values = Vec::new();
    for row in &table.rows {
        let Some(value) = row.cells.get(column) else {
            continue;
        };
        let normalized = normalize_for_match(value.trim());
        if !normalized.is_empty() {
            values.push(normalized);
        }
    }
    if values.len() < 2 {
        return false;
    }
    let unique: HashSet<&str> = values.iter().map(String::as_str).collect();
    unique.len() >= 2 && unique.len() * 10 >= values.len() * 7
}

/// 选第一个在两表中都足够唯一的非序号列作为行主键。
/// 序号列被跳过；合并类别列因重复过多不会入选，内容列会顶上。
fn choose_key_column(old: &Table, new: &Table, ignored: &[bool]) -> Option<usize> {
    (0..ignored.len()).find(|&column| {
        !ignored[column] && column_is_usable(old, column) && column_is_usable(new, column)
    })
}

struct RowView {
    cells: Vec<String>,
    text: String,
    normalized: String,
    key: Option<String>,
    features: TextFeatures,
}

fn build_view(table: &Table, ignored: &[bool], key_column: Option<usize>) -> Vec<RowView> {
    table
        .rows
        .iter()
        .map(|row| {
            let text = visible_text(row, ignored);
            let normalized = normalize_for_match(&text);
            let key = key_column
                .and_then(|column| row.cells.get(column))
                .map(|cell| normalize_for_match(cell.trim()))
                .filter(|value| !value.is_empty());
            RowView {
                cells: visible_cells(row, ignored),
                features: TextFeatures::from_normalized(&normalized),
                text,
                normalized,
                key,
            }
        })
        .collect()
}

fn visible_cells(row: &TableRow, ignored: &[bool]) -> Vec<String> {
    let mut cells: Vec<String> = row
        .cells
        .iter()
        .enumerate()
        .filter(|(column, _)| !ignored.get(*column).copied().unwrap_or(false))
        .map(|(_, cell)| cell.trim().to_string())
        .collect();
    while cells.last().is_some_and(|cell| cell.is_empty()) {
        cells.pop();
    }
    cells
}

fn visible_text(row: &TableRow, ignored: &[bool]) -> String {
    visible_cells(row, ignored).join(" | ")
}

struct RowPair {
    old: usize,
    new: usize,
    keyed: bool,
    out_of_order: bool,
    scores: RowScores,
}

#[derive(Debug, Clone, Copy)]
struct RowScores {
    text: f64,
    semantic: Option<f64>,
    total: f64,
}

fn row_scores(
    old: &RowView,
    new: &RowView,
    old_vector: Option<&[f32]>,
    new_vector: Option<&[f32]>,
    config: &SimilarityConfig,
) -> RowScores {
    let text = text_similarity(&old.features, &new.features);
    let semantic = match (old_vector, new_vector) {
        (Some(a), Some(b))
            if !a.is_empty() && !b.is_empty() && !old.text.is_empty() && !new.text.is_empty() =>
        {
            Some(cosine(a, b))
        }
        _ => None,
    };
    let total = match semantic {
        Some(value) => (config.weight_text * text + config.weight_semantic * value).clamp(0.0, 1.0),
        None => text,
    };
    RowScores {
        text,
        semantic,
        total,
    }
}

fn exact_scores(semantic_available: bool) -> RowScores {
    RowScores {
        text: 1.0,
        semantic: semantic_available.then_some(1.0),
        total: 1.0,
    }
}

fn vector_at(vectors: Option<&[Vec<f32>]>, index: usize) -> Option<&[f32]> {
    vectors?
        .get(index)
        .map(Vec::as_slice)
        .filter(|vector| !vector.is_empty())
}

fn align_rows(
    old: &[RowView],
    new: &[RowView],
    config: &SimilarityConfig,
    old_vectors: Option<&[Vec<f32>]>,
    new_vectors: Option<&[Vec<f32>]>,
) -> (Vec<RowPair>, Vec<usize>, Vec<usize>) {
    let semantic_available = old_vectors.is_some();
    let mut pairs = Vec::new();
    let mut new_used = vec![false; new.len()];
    let mut old_unmatched = Vec::new();

    // 1) 按关键列精确配对。允许跨顺序配对，行移动才能落在同一行上。
    let mut by_key: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, row) in new.iter().enumerate() {
        if let Some(key) = row.key.as_deref() {
            by_key.entry(key).or_default().push(index);
        }
    }
    let mut cursor: HashMap<&str, usize> = HashMap::new();
    for (index, row) in old.iter().enumerate() {
        let mut matched = false;
        if let Some(key) = row.key.as_deref() {
            if let Some(list) = by_key.get(key) {
                let position = cursor.entry(key).or_insert(0);
                while *position < list.len() && new_used[list[*position]] {
                    *position += 1;
                }
                if *position < list.len() {
                    let new_index = list[*position];
                    *position += 1;
                    new_used[new_index] = true;
                    pairs.push(RowPair {
                        old: index,
                        new: new_index,
                        keyed: true,
                        out_of_order: false,
                        scores: row_scores(
                            row,
                            &new[new_index],
                            vector_at(old_vectors, index),
                            vector_at(new_vectors, new_index),
                            config,
                        ),
                    });
                    matched = true;
                }
            }
        }
        if !matched {
            old_unmatched.push(index);
        }
    }

    // 2) 没有关键列或关键列被改动时，用整行文本再做一次精确配对。
    let mut by_text: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, row) in new.iter().enumerate() {
        if !new_used[index] && !row.normalized.is_empty() {
            by_text
                .entry(row.normalized.as_str())
                .or_default()
                .push(index);
        }
    }
    let mut text_cursor: HashMap<&str, usize> = HashMap::new();
    let mut still_old = Vec::new();
    for index in old_unmatched {
        let key = old[index].normalized.as_str();
        let mut matched = None;
        if !key.is_empty() {
            if let Some(list) = by_text.get(key) {
                let position = text_cursor.entry(key).or_insert(0);
                while *position < list.len() && new_used[list[*position]] {
                    *position += 1;
                }
                if *position < list.len() {
                    let new_index = list[*position];
                    *position += 1;
                    new_used[new_index] = true;
                    matched = Some(new_index);
                }
            }
        }
        match matched {
            Some(new_index) => pairs.push(RowPair {
                old: index,
                new: new_index,
                keyed: false,
                out_of_order: false,
                scores: exact_scores(semantic_available),
            }),
            None => still_old.push(index),
        }
    }
    let remaining_new: Vec<usize> = (0..new.len()).filter(|index| !new_used[*index]).collect();

    // 3) 其余行按保序相似度配对，处理改写、拆分与新增删除。
    let floor = config.threshold_suspected.clamp(0.20, 0.80);
    let (n, m) = (still_old.len(), remaining_new.len());
    let mut dp = vec![vec![0.0f64; m + 1]; n + 1];
    let mut choice = vec![vec![0u8; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            let skip_old = dp[i - 1][j];
            let skip_new = dp[i][j - 1];
            let old_row = &old[still_old[i - 1]];
            let new_row = &new[remaining_new[j - 1]];
            let scores = row_scores(
                old_row,
                new_row,
                vector_at(old_vectors, still_old[i - 1]),
                vector_at(new_vectors, remaining_new[j - 1]),
                config,
            );
            // 关键列被改写时整行相似度可能很低，但多数单元格相同仍是同一行的强信号。
            let mostly_equal = mostly_equal_cells(&old_row.cells, &new_row.cells);
            let pair = if scores.total >= floor || mostly_equal {
                dp[i - 1][j - 1] + scores.total.max(floor)
            } else {
                f64::NEG_INFINITY
            };
            if pair >= skip_old && pair >= skip_new {
                dp[i][j] = pair;
                choice[i][j] = 2;
            } else if skip_old >= skip_new {
                dp[i][j] = skip_old;
                choice[i][j] = 0;
            } else {
                dp[i][j] = skip_new;
                choice[i][j] = 1;
            }
        }
    }

    let mut matched_old = vec![false; n];
    let mut matched_new = vec![false; m];
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        match choice[i][j] {
            2 => {
                matched_old[i - 1] = true;
                matched_new[j - 1] = true;
                let old_index = still_old[i - 1];
                let new_index = remaining_new[j - 1];
                pairs.push(RowPair {
                    old: old_index,
                    new: new_index,
                    keyed: false,
                    out_of_order: false,
                    scores: row_scores(
                        &old[old_index],
                        &new[new_index],
                        vector_at(old_vectors, old_index),
                        vector_at(new_vectors, new_index),
                        config,
                    ),
                });
                i -= 1;
                j -= 1;
            }
            0 => i -= 1,
            _ => j -= 1,
        }
    }

    let remaining_old = still_old
        .iter()
        .enumerate()
        .filter(|(position, _)| !matched_old[*position])
        .map(|(_, index)| *index)
        .collect();
    let remaining_new = remaining_new
        .iter()
        .enumerate()
        .filter(|(position, _)| !matched_new[*position])
        .map(|(_, index)| *index)
        .collect();

    // 4) 保序序列里被跳过的配对即行移动；后面按内容相同/改动分别处理。
    pairs.sort_by_key(|pair| (pair.old, pair.new));
    let order: Vec<usize> = pairs.iter().map(|pair| pair.new).collect();
    let in_order = longest_increasing_members(&order);
    for (index, pair) in pairs.iter_mut().enumerate() {
        pair.out_of_order = !in_order[index];
    }
    (pairs, remaining_old, remaining_new)
}

/// 返回每个位置是否属于一条最长递增子序列。用于区分“插入导致整体顺移”和“真正的行移动”。
fn longest_increasing_members(values: &[usize]) -> Vec<bool> {
    let mut tails: Vec<usize> = Vec::new();
    let mut previous: Vec<Option<usize>> = vec![None; values.len()];
    for (index, &value) in values.iter().enumerate() {
        let position = tails.partition_point(|&tail| values[tail] < value);
        if position > 0 {
            previous[index] = Some(tails[position - 1]);
        }
        if position == tails.len() {
            tails.push(index);
        } else {
            tails[position] = index;
        }
    }
    let mut members = vec![false; values.len()];
    let mut cursor = tails.last().copied();
    while let Some(index) = cursor {
        members[index] = true;
        cursor = previous[index];
    }
    members
}

fn compare_table_pair(
    old: &Table,
    new: &Table,
    config: &Config,
    old_vectors: Option<&[Vec<f32>]>,
    new_vectors: Option<&[Vec<f32>]>,
) -> Vec<Record> {
    let ignored = ignored_columns(old, new);
    let key_column = choose_key_column(old, new, &ignored);
    let old_rows = build_view(old, &ignored, key_column);
    let new_rows = build_view(new, &ignored, key_column);
    let semantic_available = old_vectors.is_some();
    let (pairs, removed_rows, added_rows) = align_rows(
        &old_rows,
        &new_rows,
        &config.similarity,
        old_vectors,
        new_vectors,
    );
    let table_number = new.index + 1;
    let caption = new.caption.clone().or_else(|| old.caption.clone());
    let mut records = Vec::new();

    for pair in pairs {
        let old_row = &old_rows[pair.old];
        let new_row = &new_rows[pair.new];
        if old_row.normalized == new_row.normalized {
            if pair.out_of_order {
                records.push(table_move_record(
                    old.rows[pair.old].index,
                    new.rows[pair.new].index,
                    new_row.text.clone(),
                    new.anchor,
                    table_number,
                    caption.clone(),
                    semantic_available,
                ));
            }
            continue;
        }
        let mostly_equal = mostly_equal_cells(&old_row.cells, &new_row.cells);
        let confirmed =
            pair.keyed || pair.scores.total >= config.similarity.threshold_modified || mostly_equal;
        let status = if confirmed {
            Status::Modified
        } else {
            Status::SuspectedModified
        };
        let mut reasons = Vec::new();
        if pair.out_of_order {
            reasons.push("行序变化".to_string());
        }
        if pair.keyed {
            reasons.push("关键列相同，按同一行比较".to_string());
        } else if confirmed
            && mostly_equal
            && pair.scores.total < config.similarity.threshold_modified
        {
            reasons.push("多数单元格相同，按同一行确认修改".to_string());
        }
        if status == Status::SuspectedModified {
            reasons.push("疑似修改，低于确认阈值".to_string());
        }
        let mut note = reasons.join("；");
        note.push_str(&caption_suffix(caption.as_deref()));
        records.push(Record {
            id: 0,
            status,
            old_para_index: None,
            new_para_index: None,
            old_text: Some(old_row.text.clone()),
            new_text: Some(new_row.text.clone()),
            scores: Scores {
                text: pair.scores.text,
                semantic: pair.scores.semantic,
                total: pair.scores.total,
            },
            diff: None,
            matched_to: Some(new.rows[pair.new].index + 1),
            note: (!note.is_empty()).then_some(note),
            table: Some(TableLocation {
                table: table_number,
                old_row: Some(old.rows[pair.old].index + 1),
                new_row: Some(new.rows[pair.new].index + 1),
                caption: caption.clone(),
            }),
            order: row_order(new.anchor, new.rows[pair.new].index),
        });
    }

    for index in removed_rows {
        let row = &old_rows[index];
        records.push(Record {
            id: 0,
            status: Status::Removed,
            old_para_index: None,
            new_para_index: None,
            old_text: Some(row.text.clone()),
            new_text: None,
            scores: Scores::empty(),
            diff: None,
            matched_to: None,
            note: Some(format!("整行删除{}", caption_suffix(caption.as_deref()))),
            table: Some(TableLocation {
                table: table_number,
                old_row: Some(old.rows[index].index + 1),
                new_row: None,
                caption: caption.clone(),
            }),
            order: row_order(new.anchor, old.rows[index].index),
        });
    }

    for index in added_rows {
        let row = &new_rows[index];
        records.push(Record {
            id: 0,
            status: Status::Added,
            old_para_index: None,
            new_para_index: None,
            old_text: None,
            new_text: Some(row.text.clone()),
            scores: Scores::empty(),
            diff: None,
            matched_to: None,
            note: Some(format!("整行新增{}", caption_suffix(caption.as_deref()))),
            table: Some(TableLocation {
                table: table_number,
                old_row: None,
                new_row: Some(new.rows[index].index + 1),
                caption: caption.clone(),
            }),
            order: row_order(new.anchor, new.rows[index].index),
        });
    }

    records
}

fn table_move_record(
    old_index: usize,
    new_index: usize,
    text: String,
    anchor: usize,
    table_number: usize,
    caption: Option<String>,
    semantic_available: bool,
) -> Record {
    Record {
        id: 0,
        status: Status::Moved,
        old_para_index: None,
        new_para_index: None,
        old_text: Some(text.clone()),
        new_text: Some(text),
        scores: Scores::exact(semantic_available),
        diff: None,
        matched_to: Some(new_index + 1),
        note: Some("内容相同，行顺序变化".to_string()),
        table: Some(TableLocation {
            table: table_number,
            old_row: Some(old_index + 1),
            new_row: Some(new_index + 1),
            caption,
        }),
        order: row_order(anchor, new_index),
    }
}

fn whole_table_record(table: &Table, added: bool) -> Record {
    let text = table
        .rows
        .iter()
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let caption = table.caption.clone();
    let verb = if added {
        "整表新增"
    } else {
        "整表删除"
    };
    Record {
        id: 0,
        status: if added {
            Status::Added
        } else {
            Status::Removed
        },
        old_para_index: None,
        new_para_index: None,
        old_text: (!added).then(|| text.clone()),
        new_text: added.then(|| text),
        scores: Scores::empty(),
        diff: None,
        matched_to: None,
        note: Some(format!(
            "{verb}，共 {} 行{}",
            table.rows.len(),
            caption_suffix(caption.as_deref())
        )),
        table: Some(TableLocation {
            table: table.index + 1,
            old_row: None,
            new_row: None,
            caption,
        }),
        order: table.anchor as f64 - 0.35,
    }
}

fn mostly_equal_cells(old: &[String], new: &[String]) -> bool {
    let aligned = old.len().min(new.len());
    let mut meaningful = 0;
    let mut equal = 0;
    for index in 0..aligned {
        if old[index].is_empty() && new[index].is_empty() {
            continue;
        }
        meaningful += 1;
        if normalize_for_match(&old[index]) == normalize_for_match(&new[index]) {
            equal += 1;
        }
    }
    meaningful > 0 && equal * 2 > meaningful
}

fn row_order(anchor: usize, row_index: usize) -> f64 {
    anchor as f64 - 0.4 + (row_index as f64 + 1.0) * 1e-4
}

fn caption_suffix(caption: Option<&str>) -> String {
    match caption {
        Some(caption) => {
            let mut text: String = caption.chars().take(30).collect();
            if caption.chars().count() > 30 {
                text.push('…');
            }
            format!("；表标题：{text}")
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::{Table, TableRow};
    use crate::normalize::normalize_for_match;

    fn table(index: usize, anchor: usize, rows: &[&[&str]]) -> Table {
        let rows = rows
            .iter()
            .enumerate()
            .map(|(row_index, cells)| {
                let cells: Vec<String> = cells.iter().map(|cell| cell.to_string()).collect();
                let text = cells.join(" | ");
                let normalized = normalize_for_match(&text);
                let key = cells
                    .first()
                    .map(|cell| normalize_for_match(cell))
                    .filter(|key| !key.is_empty());
                TableRow {
                    index: row_index,
                    cells,
                    text,
                    normalized,
                    key,
                }
            })
            .collect();
        Table {
            index,
            caption: None,
            anchor,
            rows,
        }
    }

    fn run(old: &Table, new: &Table) -> Vec<Record> {
        match_tables(
            std::slice::from_ref(old),
            std::slice::from_ref(new),
            &Config::default(),
            None,
        )
    }

    fn run_with_vectors(
        old: &Table,
        new: &Table,
        vectors: &TableVectors,
        config: &Config,
    ) -> Vec<Record> {
        match_tables(
            std::slice::from_ref(old),
            std::slice::from_ref(new),
            config,
            Some(vectors),
        )
    }

    #[test]
    fn keyed_row_change_is_modified() {
        let old = table(
            0,
            1,
            &[&["指标", "本月"], &["售电量", "100"], &["电价", "0.6"]],
        );
        let new = table(
            0,
            1,
            &[&["指标", "本月"], &["售电量", "120"], &["电价", "0.6"]],
        );
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Modified);
        let location = records[0].table.as_ref().unwrap();
        assert_eq!(location.table, 1);
        assert_eq!(location.old_row, Some(2));
        assert_eq!(location.new_row, Some(2));
    }

    #[test]
    fn inserted_row_is_added_without_mass_modification() {
        let old = table(0, 1, &[&["指标", "值"], &["甲", "1"], &["乙", "2"]]);
        let new = table(
            0,
            1,
            &[&["指标", "值"], &["甲", "1"], &["新项", "9"], &["乙", "2"]],
        );
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Added);
        assert_eq!(records[0].table.as_ref().unwrap().new_row, Some(3));
    }

    #[test]
    fn removed_row_is_removed() {
        let old = table(0, 1, &[&["指标", "值"], &["甲", "1"], &["乙", "2"]]);
        let new = table(0, 1, &[&["指标", "值"], &["乙", "2"]]);
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Removed);
        assert_eq!(records[0].table.as_ref().unwrap().old_row, Some(2));
    }

    #[test]
    fn unchanged_table_emits_nothing() {
        let old = table(0, 1, &[&["指标", "值"], &["甲", "1"]]);
        let new = table(0, 1, &[&["指标", "值"], &["甲", "1"]]);
        assert!(run(&old, &new).is_empty());
    }

    #[test]
    fn whole_table_added_is_one_record() {
        let existing = table(0, 1, &[&["指标", "值"], &["甲", "1"], &["乙", "2"]]);
        let extra = table(1, 3, &[&["姓名", "部门"], &["丙", "办公室"]]);
        let records = match_tables(
            std::slice::from_ref(&existing),
            &[existing.clone(), extra],
            &Config::default(),
            None,
        );
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Added);
        assert_eq!(records[0].table.as_ref().unwrap().table, 2);
        assert!(records[0].note.as_deref().unwrap().contains("整表新增"));
    }

    #[test]
    fn merged_category_single_value_change_is_confirmed() {
        let old = table(
            0,
            1,
            &[
                &["类别", "指标", "值"],
                &["营销", "售电量", "100"],
                &["营销", "电价", "0.60"],
                &["营销", "投诉", "3"],
                &["运检", "抢修", "5"],
            ],
        );
        let new = table(
            0,
            1,
            &[
                &["类别", "指标", "值"],
                &["营销", "售电量", "120"],
                &["营销", "电价", "0.60"],
                &["营销", "投诉", "3"],
                &["运检", "抢修", "5"],
            ],
        );
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Modified);
        assert!(
            records[0]
                .note
                .as_deref()
                .is_some_and(|note| note.contains("关键列相同")),
            "{records:#?}"
        );
    }

    #[test]
    fn serial_renumbering_is_not_a_modification() {
        let old = table(
            0,
            1,
            &[
                &["序号", "事项"],
                &["1", "甲项内容"],
                &["2", "乙项内容"],
                &["3", "丙项内容"],
            ],
        );
        let new = table(
            0,
            1,
            &[
                &["序号", "事项"],
                &["1", "甲项内容"],
                &["2", "新插入项"],
                &["3", "乙项内容"],
                &["4", "丙项内容"],
            ],
        );
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Added);
        assert_eq!(records[0].table.as_ref().unwrap().new_row, Some(3));
    }

    #[test]
    fn moved_row_is_reported_as_moved() {
        let old = table(
            0,
            1,
            &[
                &["序号", "事项"],
                &["1", "甲"],
                &["2", "乙"],
                &["3", "丙"],
                &["4", "丁"],
            ],
        );
        let new = table(
            0,
            1,
            &[
                &["序号", "事项"],
                &["1", "丙"],
                &["2", "甲"],
                &["3", "乙"],
                &["4", "丁"],
            ],
        );
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Moved);
        let location = records[0].table.as_ref().unwrap();
        assert_eq!(location.old_row, Some(4));
        assert_eq!(location.new_row, Some(2));
    }

    #[test]
    fn moved_row_with_changed_category_is_one_modification() {
        let old = table(
            0,
            1,
            &[
                &["序号", "类别", "事项"],
                &["1", "甲类", "事项甲"],
                &["2", "甲类", "事项乙"],
                &["3", "乙类", "事项丙"],
            ],
        );
        let new = table(
            0,
            1,
            &[
                &["序号", "类别", "事项"],
                &["1", "甲类", "事项丙"],
                &["2", "甲类", "事项甲"],
                &["3", "甲类", "事项乙"],
            ],
        );
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Modified);
        let location = records[0].table.as_ref().unwrap();
        assert_eq!(location.old_row, Some(4));
        assert_eq!(location.new_row, Some(2));
        let note = records[0].note.as_deref().unwrap();
        assert!(note.contains("行序变化"), "{note}");
        assert!(note.contains("关键列相同"), "{note}");
    }

    #[test]
    fn mostly_equal_cells_confirms_key_change() {
        let old = table(
            0,
            1,
            &[
                &["类别", "指标", "值"],
                &["营销", "售电量", "100"],
                &["营销", "电价", "0.60"],
            ],
        );
        let new = table(
            0,
            1,
            &[
                &["类别", "指标", "值"],
                &["营销", "售电总量统计口径", "100"],
                &["营销", "电价", "0.60"],
            ],
        );
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].status, Status::Modified);
        assert!(
            records[0]
                .note
                .as_deref()
                .is_some_and(|note| note.contains("多数单元格相同")),
            "{records:#?}"
        );
    }

    #[test]
    fn row_without_keys_falls_back_to_similarity() {
        let old = table(0, 1, &[&["", "本月"], &["", "售电量本月完成值 100"]]);
        let new = table(0, 1, &[&["", "本月"], &["", "售电量本月完成值 120"]]);
        let records = run(&old, &new);
        assert_eq!(records.len(), 1, "{records:#?}");
        assert!(records[0].status.is_modified_like());
    }

    #[test]
    fn semantic_texts_skip_ordinal_column() {
        let table = table(
            0,
            1,
            &[
                &["序号", "事项", "值"],
                &["1", "甲项", "10"],
                &["2", "乙项", "20"],
                &["3", "丙项", "30"],
            ],
        );
        assert_eq!(
            semantic_texts(&table),
            vec!["事项 | 值", "甲项 | 10", "乙项 | 20", "丙项 | 30"]
        );
    }

    #[test]
    fn row_semantic_score_is_reported() {
        let old = table(0, 1, &[&["指标", "本月"], &["售电量", "100"]]);
        let new = table(0, 1, &[&["指标", "本月"], &["售电量", "120"]]);
        let vectors = TableVectors {
            old: vec![vec![vec![1.0, 0.0], vec![0.7, 0.7]]],
            new: vec![vec![vec![1.0, 0.0], vec![0.6, 0.8]]],
        };
        let records = run_with_vectors(&old, &new, &vectors, &Config::default());
        assert_eq!(records.len(), 1, "{records:#?}");
        let scores = records[0].scores.clone();
        let semantic = scores.semantic.expect("语义分应写入记录");
        assert!((semantic - 0.989949).abs() < 1e-5, "{semantic}");
        let expected = 0.6 * scores.text + 0.4 * semantic;
        assert!((scores.total - expected).abs() < 1e-9, "{scores:?}");
    }

    #[test]
    fn semantic_similarity_pairs_rewritten_rows() {
        let old = table(
            0,
            1,
            &[&["", "苹果香蕉梨", "甲"], &["", "电压电流功率", "乙"]],
        );
        let new = table(
            0,
            1,
            &[&["", "葡萄柚子橙", "甲"], &["", "千瓦安培伏特", "乙"]],
        );
        let vectors = TableVectors {
            old: vec![vec![vec![1.0, 0.0], vec![0.0, 1.0]]],
            new: vec![vec![vec![1.0, 0.0], vec![0.0, 1.0]]],
        };
        let mut config = Config::default();
        config.similarity.weight_text = 0.2;
        config.similarity.weight_semantic = 0.8;
        config.similarity.threshold_modified = 0.6;
        config.similarity.threshold_suspected = 0.6;
        let records = run_with_vectors(&old, &new, &vectors, &config);
        assert_eq!(records.len(), 2, "{records:#?}");
        for record in &records {
            assert_eq!(record.status, Status::Modified, "{record:#?}");
            assert!(record.scores.text < 0.3, "{:?}", record.scores);
            assert!(
                record.scores.semantic.is_some_and(|value| value > 0.99),
                "{:?}",
                record.scores
            );
        }
    }

    #[test]
    fn empty_vectors_leave_semantic_empty() {
        let old = table(0, 1, &[&["指标", "本月"], &["售电量", "100"]]);
        let new = table(0, 1, &[&["指标", "本月"], &["售电量", "120"]]);
        let vectors = TableVectors {
            old: vec![vec![Vec::new(), Vec::new()]],
            new: vec![vec![Vec::new(), Vec::new()]],
        };
        let records = run_with_vectors(&old, &new, &vectors, &Config::default());
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].scores.semantic, None);
        assert_eq!(records[0].scores.total, records[0].scores.text);
    }
}
