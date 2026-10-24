pub mod table;
pub mod text_sim;

use std::collections::HashMap;

use crate::config::Config;
use crate::docx::Paragraph;
use crate::result::{Record, Scores, Status};

use text_sim::{TextFeatures, cosine, text_similarity};

#[derive(Debug, Clone)]
pub enum Progress {
    Stage(String),
    Step {
        label: String,
        done: usize,
        total: usize,
    },
}

pub struct SemanticVectors {
    pub old: Vec<Vec<f32>>,
    pub new: Vec<Vec<f32>>,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    old: usize,
    new: usize,
    text: f64,
    semantic: Option<f64>,
    total: f64,
}

pub fn match_documents(
    old: &[Paragraph],
    new: &[Paragraph],
    config: &Config,
    semantic: Option<&SemanticVectors>,
    progress: &mut dyn FnMut(Progress),
) -> Vec<Record> {
    progress(Progress::Stage("段落匹配".to_string()));
    let semantic_available = semantic.is_some();
    let mut old_used = vec![false; old.len()];
    let mut new_used = vec![false; new.len()];
    let mut records: Vec<Record> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();

    // 1) 精确匹配（归一化后全文相同）优先，用于识别未变与移动。
    let mut new_by_text: HashMap<&str, Vec<usize>> = HashMap::new();
    for paragraph in new {
        new_by_text
            .entry(paragraph.normalized.as_str())
            .or_default()
            .push(paragraph.index);
    }
    let mut cursor: HashMap<&str, usize> = HashMap::new();
    let mut exact: Vec<(usize, usize)> = Vec::new();
    for paragraph in old {
        let key = paragraph.normalized.as_str();
        if let Some(list) = new_by_text.get(key) {
            let position = cursor.entry(key).or_insert(0);
            if *position < list.len() {
                exact.push((paragraph.index, list[*position]));
                *position += 1;
            }
        }
    }
    exact.sort_unstable_by_key(|(old_index, _)| *old_index);
    let lis = lis_members(
        &exact
            .iter()
            .map(|(_, new_index)| *new_index)
            .collect::<Vec<_>>(),
    );
    let mut anchors: Vec<(usize, usize)> = Vec::new();

    for (position, (old_index, new_index)) in exact.iter().copied().enumerate() {
        if old_used[old_index] || new_used[new_index] {
            continue;
        }
        old_used[old_index] = true;
        new_used[new_index] = true;
        pairs.push((old_index, new_index));
        let in_place = old_index == new_index || lis[position];
        if in_place {
            anchors.push((old_index, new_index));
        }
        let status = if in_place {
            Status::Unchanged
        } else {
            Status::Moved
        };
        records.push(Record {
            id: 0,
            status,
            old_para_index: Some(old_index + 1),
            new_para_index: Some(new_index + 1),
            old_text: Some(old[old_index].text.clone()),
            new_text: Some(new[new_index].text.clone()),
            scores: Scores::exact(semantic_available),
            diff: None,
            row_diff: None,
            matched_to: Some(new_index + 1),
            note: (status == Status::Moved).then(|| "内容相同，位置发生变化".to_string()),
            table: None,
            order: 0.0,
        });
    }

    // 2) 窗口内文本相似度初筛 + 语义确认，双向唯一贪心配对。
    let old_features: Vec<TextFeatures> = old
        .iter()
        .map(|p| TextFeatures::from_normalized(&p.normalized))
        .collect();
    let new_features: Vec<TextFeatures> = new
        .iter()
        .map(|p| TextFeatures::from_normalized(&p.normalized))
        .collect();

    let cfg = &config.similarity;
    let mut candidates: Vec<Candidate> = Vec::new();
    for old_index in 0..old.len() {
        if old_used[old_index] {
            continue;
        }
        let predicted = predict_new_position(old_index, &anchors, new.len());
        let start = predicted.saturating_sub(cfg.window);
        let end = (predicted + cfg.window + 1).min(new.len());
        for new_index in start..end {
            if new_used[new_index] {
                continue;
            }
            let text = text_similarity(&old_features[old_index], &new_features[new_index]);
            if text < cfg.text_candidate_threshold {
                continue;
            }
            let semantic_score =
                semantic.map(|vectors| cosine(&vectors.old[old_index], &vectors.new[new_index]));
            let total = match semantic_score {
                Some(semantic_score) => {
                    (cfg.weight_text * text + cfg.weight_semantic * semantic_score).clamp(0.0, 1.0)
                }
                None => text,
            };
            if total < cfg.threshold_suspected {
                continue;
            }
            candidates.push(Candidate {
                old: old_index,
                new: new_index,
                text,
                semantic: semantic_score,
                total,
            });
        }
    }

    let mut candidates_by_old: Vec<Vec<usize>> = vec![Vec::new(); old.len()];
    let mut candidates_by_new: Vec<Vec<usize>> = vec![Vec::new(); new.len()];
    for candidate in &candidates {
        candidates_by_old[candidate.old].push(candidate.new);
        candidates_by_new[candidate.new].push(candidate.old);
    }
    let old_lengths: Vec<usize> = old
        .iter()
        .map(|p| p.normalized.chars().count().max(1))
        .collect();
    let new_lengths: Vec<usize> = new
        .iter()
        .map(|p| p.normalized.chars().count().max(1))
        .collect();

    // 一个段落被相邻两段“分摊”且长度大致相等时，按拆分/合并处理（删除+新增）。
    let mut split_old = vec![false; old.len()];
    for (old_index, list) in candidates_by_old.iter_mut().enumerate() {
        list.sort_unstable();
        list.dedup();
        split_old[old_index] = list.windows(2).any(|pair| {
            pair[1] == pair[0] + 1
                && looks_like_split(
                    old_lengths[old_index],
                    new_lengths[pair[0]],
                    new_lengths[pair[1]],
                )
        });
    }
    let mut split_new = vec![false; new.len()];
    for (new_index, list) in candidates_by_new.iter_mut().enumerate() {
        list.sort_unstable();
        list.dedup();
        split_new[new_index] = list.windows(2).any(|pair| {
            pair[1] == pair[0] + 1
                && looks_like_split(
                    new_lengths[new_index],
                    old_lengths[pair[0]],
                    old_lengths[pair[1]],
                )
        });
    }

    candidates.sort_by(|a, b| {
        b.total
            .total_cmp(&a.total)
            .then_with(|| b.text.total_cmp(&a.text))
            .then_with(|| a.old.abs_diff(a.new).cmp(&b.old.abs_diff(b.new)))
            .then_with(|| a.old.cmp(&b.old))
            .then_with(|| a.new.cmp(&b.new))
    });

    for candidate in candidates {
        if split_old[candidate.old] || split_new[candidate.new] {
            continue;
        }
        if old_used[candidate.old] || new_used[candidate.new] {
            continue;
        }
        old_used[candidate.old] = true;
        new_used[candidate.new] = true;
        pairs.push((candidate.old, candidate.new));
        let status = if candidate.total >= cfg.threshold_modified {
            Status::Modified
        } else {
            Status::SuspectedModified
        };
        records.push(Record {
            id: 0,
            status,
            old_para_index: Some(candidate.old + 1),
            new_para_index: Some(candidate.new + 1),
            old_text: Some(old[candidate.old].text.clone()),
            new_text: Some(new[candidate.new].text.clone()),
            scores: Scores {
                text: candidate.text,
                semantic: candidate.semantic,
                total: candidate.total,
            },
            diff: None,
            row_diff: None,
            matched_to: Some(candidate.new + 1),
            note: (status == Status::SuspectedModified)
                .then(|| "疑似修改，低于确认阈值".to_string()),
            table: None,
            order: 0.0,
        });
    }

    // 3) 未匹配段落：旧侧删除、新侧新增。
    for (index, paragraph) in old.iter().enumerate() {
        if !old_used[index] {
            records.push(Record {
                id: 0,
                status: Status::Removed,
                old_para_index: Some(paragraph.index + 1),
                new_para_index: None,
                old_text: Some(paragraph.text.clone()),
                new_text: None,
                scores: Scores::empty(),
                diff: None,
                row_diff: None,
                matched_to: None,
                note: None,
                table: None,
                order: 0.0,
            });
        }
    }
    for (index, paragraph) in new.iter().enumerate() {
        if !new_used[index] {
            records.push(Record {
                id: 0,
                status: Status::Added,
                old_para_index: None,
                new_para_index: Some(paragraph.index + 1),
                old_text: None,
                new_text: Some(paragraph.text.clone()),
                scores: Scores::empty(),
                diff: None,
                row_diff: None,
                matched_to: None,
                note: None,
                table: None,
                order: 0.0,
            });
        }
    }

    // 4) 按新文档阅读顺序排列；删除记录插到其旧位置对应的新位置。
    pairs.sort_unstable();
    let mut keyed: Vec<(f64, u8, usize, Record)> = records
        .into_iter()
        .map(|mut record| {
            let key = match (record.old_para_index, record.new_para_index) {
                (_, Some(new_index)) => (f64::from(new_index as u32) - 1.0, 1u8, record.id),
                (Some(old_index), None) => {
                    let old_index = old_index - 1;
                    let previous = pairs
                        .iter()
                        .take_while(|(old, _)| *old < old_index)
                        .last()
                        .map(|(_, new)| *new as f64 + 0.5);
                    let position = previous.unwrap_or_else(|| {
                        pairs
                            .iter()
                            .find(|(old, _)| *old > old_index)
                            .map(|(_, new)| *new as f64 - 0.5)
                            .unwrap_or(-0.5)
                    });
                    (position, 0u8, old_index)
                }
                (None, None) => (-0.5, 2, 0),
            };
            record.order = key.0 + f64::from(key.1) * 1e-3 + key.2 as f64 * 1e-9;
            (key.0, key.1, key.2, record)
        })
        .collect();
    keyed.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut ordered: Vec<Record> = keyed.into_iter().map(|(_, _, _, record)| record).collect();
    for (index, record) in ordered.iter_mut().enumerate() {
        record.id = index + 1;
    }
    ordered
}

fn predict_new_position(old_index: usize, anchors: &[(usize, usize)], new_len: usize) -> usize {
    if new_len == 0 {
        return 0;
    }
    let position = anchors.partition_point(|(old, _)| *old < old_index);
    let previous = position.checked_sub(1).map(|index| {
        let (anchor_old, anchor_new) = anchors[index];
        (
            old_index - anchor_old,
            anchor_new + (old_index - anchor_old),
        )
    });
    let next = anchors.get(position).map(|(anchor_old, anchor_new)| {
        let distance = anchor_old - old_index;
        (distance, anchor_new.saturating_sub(distance))
    });
    let predicted = match (previous, next) {
        (
            Some((previous_distance, previous_prediction)),
            Some((next_distance, next_prediction)),
        ) => {
            if previous_distance <= next_distance {
                previous_prediction
            } else {
                next_prediction
            }
        }
        (Some((_, prediction)), None) | (None, Some((_, prediction))) => prediction,
        (None, None) => old_index,
    };
    predicted.min(new_len - 1)
}

fn looks_like_split(target: usize, first: usize, second: usize) -> bool {
    let combined = first + second;
    let target = target as f64;
    let combined = combined as f64;
    let first = first as f64;
    let second = second as f64;
    combined / target >= 0.75
        && combined / target <= 1.45
        && first <= target * 0.85
        && second <= target * 0.85
}

fn lis_members(values: &[usize]) -> Vec<bool> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![usize::MAX; values.len()];
    for (index, value) in values.iter().copied().enumerate() {
        let position = tails.partition_point(|&tail| values[tail] < value);
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
    let mut current = *tails.last().unwrap();
    loop {
        members[current] = true;
        if previous[current] == usize::MAX {
            break;
        }
        current = previous[current];
    }
    members
}

#[cfg(test)]
mod tests {
    use crate::docx::Paragraph;
    use crate::normalize::normalize_for_match;

    use super::*;

    fn paras(values: &[&str]) -> Vec<Paragraph> {
        values
            .iter()
            .enumerate()
            .map(|(index, text)| Paragraph {
                index,
                text: (*text).to_string(),
                normalized: normalize_for_match(text),
            })
            .collect()
    }

    fn run_with(old: &[&str], new: &[&str], config: &Config) -> Vec<Record> {
        let old = paras(old);
        let new = paras(new);
        match_documents(&old, &new, config, None, &mut |_| {})
    }

    fn run(old: &[&str], new: &[&str]) -> Vec<Record> {
        run_with(old, new, &Config::default())
    }

    fn status_of(records: &[Record], status: Status) -> Vec<&Record> {
        records.iter().filter(|r| r.status == status).collect()
    }

    #[test]
    fn detects_unchanged_moved_added_removed() {
        let records = run(
            &["第一段内容相同", "第二段内容相同", "将被删除的段落"],
            &["第二段内容相同", "第一段内容相同", "新增的段落"],
        );
        assert!(
            status_of(&records, Status::Moved).len() == 1,
            "{records:#?}"
        );
        assert_eq!(status_of(&records, Status::Unchanged).len(), 1);
        assert_eq!(status_of(&records, Status::Added).len(), 1);
        assert_eq!(status_of(&records, Status::Removed).len(), 1);
    }

    #[test]
    fn insertion_does_not_mark_everything_moved() {
        let records = run(
            &["甲段内容", "乙段内容", "丙段内容"],
            &["新增开头", "甲段内容", "乙段内容", "丙段内容"],
        );
        let moved = records.iter().filter(|r| r.status == Status::Moved).count();
        assert_eq!(moved, 0, "{records:#?}");
        assert_eq!(
            records
                .iter()
                .filter(|r| r.status == Status::Unchanged)
                .count(),
            3
        );
    }

    #[test]
    fn classifies_modified_and_suspected_by_thresholds() {
        let mut config = Config::default();
        config.similarity.threshold_modified = 0.95;
        config.similarity.threshold_suspected = 0.3;
        config.similarity.text_candidate_threshold = 0.2;
        let records = run_with(
            &["公司应当每年开展一次安全检查并报送结果。"],
            &["公司应当每季度开展一次安全检查并报送结果。"],
            &config,
        );
        assert_eq!(records[0].status, Status::SuspectedModified);
        assert!(records[0].scores.total >= 0.3);
        assert!(records[0].scores.total < 0.95);

        config.similarity.threshold_modified = 0.5;
        let records = run_with(
            &["公司应当每年开展一次安全检查并报送结果。"],
            &["公司应当每季度开展一次安全检查并报送结果。"],
            &config,
        );
        assert_eq!(records[0].status, Status::Modified);
    }

    #[test]
    fn window_tracks_offset_after_many_deletions() {
        let records = run(
            &[
                "固定段落一。",
                "删除内容一。",
                "删除内容二。",
                "删除内容三。",
                "删除内容四。",
                "删除内容五。",
                "删除内容六。",
                "删除内容七。",
                "删除内容八。",
                "公司应当建立安全管理制度。",
                "尾部段落内容。",
            ],
            &[
                "固定段落一。",
                "公司应当建立安全生产管理制度。",
                "尾部段落内容。",
            ],
        );
        let modified: Vec<&Record> = records
            .iter()
            .filter(|record| record.status.is_modified_like())
            .collect();
        assert_eq!(modified.len(), 1, "{records:#?}");
        assert_eq!(modified[0].old_para_index, Some(10));
        assert_eq!(modified[0].new_para_index, Some(2));
    }

    #[test]
    fn repeated_boilerplate_is_not_mistaken_for_split() {
        let base = "公司应当建立安全生产管理制度，各部门按照职责分工开展隐患排查治理。";
        let old = [
            format!("{base}第一段。"),
            format!("{base}第二段。"),
            format!("{base}第三段。"),
        ];
        let new = [
            format!("{base}第一段。"),
            format!("{base}第二段，并定期报送结果。"),
            format!("{base}第三段。"),
        ];
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();
        let records = run(&old_refs, &new_refs);
        assert_eq!(
            status_of(&records, Status::Modified).len(),
            1,
            "{records:#?}"
        );
        assert_eq!(status_of(&records, Status::Removed).len(), 0);
        assert_eq!(status_of(&records, Status::Added).len(), 0);
    }

    #[test]
    fn split_and_merge_become_remove_plus_add() {
        let records = run(
            &["这是一段很长的制度内容，包含甲乙丙丁多个要点。"],
            &["这是一段很长的制度内容，包含甲乙丙丁多个要点。第一部分"],
        );
        // 相似度很高，仍会配对；真正的拆分/合并是内容差异足够大时的兜底行为。
        assert_eq!(records.len(), 1);

        let records = run(
            &["本规定适用于公司本部及所属各单位。"],
            &["本规定适用于公司本部。", "本规定适用于所属各单位。"],
        );
        assert!(
            records.iter().any(|r| r.status == Status::Removed),
            "{records:#?}"
        );
        assert!(records.iter().any(|r| r.status == Status::Added));
    }

    #[test]
    fn numbering_is_ignored_for_matching() {
        let records = run(
            &["第十二条 本规定由办公室负责解释。"],
            &["第二十条 本规定由办公室负责解释。"],
        );
        assert_eq!(records[0].status, Status::Unchanged);
    }

    #[test]
    fn removed_records_are_interleaved_by_position() {
        let records = run(&["甲", "删除一", "乙", "删除二", "丙"], &["甲", "乙", "丙"]);
        let statuses: Vec<Status> = records.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            vec![
                Status::Unchanged,
                Status::Removed,
                Status::Unchanged,
                Status::Removed,
                Status::Unchanged
            ]
        );
    }
}
