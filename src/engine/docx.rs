use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use crate::config::Config;
use crate::diff::para_diff;
use crate::docx;
use crate::matching::table::TableVectors;
use crate::matching::{self, Progress, SemanticVectors};
use crate::model::Embedder;
use crate::result::{CompareOutput, SemanticStatus, Status, Summary};

const EMBED_CHUNK: usize = 64;

struct PreparedSemantic {
    paragraphs: SemanticVectors,
    tables: TableVectors,
}

pub fn compare(
    old_path: &Path,
    new_path: &Path,
    config: &Config,
    progress: &mut dyn FnMut(Progress),
) -> Result<CompareOutput> {
    let started = Instant::now();
    progress(Progress::Stage("解析文档".to_string()));
    let old = docx::parse(old_path)?;
    let new = docx::parse(new_path)?;
    log::info!(
        "解析完成：旧 {} 段/{} 表，新 {} 段/{} 表",
        old.paragraphs.len(),
        old.tables.len(),
        new.paragraphs.len(),
        new.tables.len()
    );

    let mut semantic_status = SemanticStatus::disabled("配置中已关闭语义匹配");
    let mut prepared: Option<PreparedSemantic> = None;

    if config.model.enabled {
        match prepare_semantic(config, &old, &new, progress) {
            Ok(Some((status, value))) => {
                semantic_status = status;
                prepared = Some(value);
            }
            Ok(None) => {
                semantic_status = SemanticStatus::disabled("没有可比较的段落或表格行");
            }
            Err(err) => {
                if config.model.required {
                    return Err(err);
                }
                log::warn!("语义匹配不可用，退化为纯文本相似度：{err:#}");
                semantic_status = SemanticStatus::disabled(format!("{err:#}"));
            }
        }
    }

    progress(Progress::Stage("段落匹配".to_string()));
    let mut records = matching::match_documents(
        &old.paragraphs,
        &new.paragraphs,
        config,
        prepared.as_ref().map(|value| &value.paragraphs),
        progress,
    );

    progress(Progress::Stage("表格匹配".to_string()));
    let (table_records, tables) = matching::table::match_tables(
        &old.tables,
        &new.tables,
        config,
        prepared.as_ref().map(|value| &value.tables),
    );
    records.extend(table_records);
    records.sort_by(|a, b| a.order.total_cmp(&b.order));
    for (index, record) in records.iter_mut().enumerate() {
        record.id = index + 1;
    }

    progress(Progress::Stage("生成字符级差异".to_string()));
    let modified_count = records
        .iter()
        .filter(|record| record.status.is_modified_like())
        .count();
    let record_count = records.len();
    for (index, record) in records.iter_mut().enumerate() {
        if record.status.is_modified_like() {
            let old_text = record.old_text.as_deref().unwrap_or("");
            let new_text = record.new_text.as_deref().unwrap_or("");
            record.diff = Some(para_diff(old_text, new_text));
            if let Some(cells) = record.row_diff.as_mut() {
                for cell in cells {
                    if let (Some(old), Some(new)) = (cell.old.as_deref(), cell.new.as_deref()) {
                        cell.diff = Some(para_diff(old, new));
                    }
                }
            }
            if modified_count > 20 && index % 20 == 0 {
                progress(Progress::Step {
                    label: "段内 diff".to_string(),
                    done: index,
                    total: record_count,
                });
            }
        }
    }

    let summary = Summary::from_records(&records);
    let elapsed = started.elapsed();
    log::info!(
        "比较完成：{}，用时 {:.2}s",
        summary.brief(),
        elapsed.as_secs_f64()
    );
    log_summary(&records, elapsed);

    Ok(CompareOutput {
        old_path: old_path.to_path_buf(),
        new_path: new_path.to_path_buf(),
        old_paragraphs: old.paragraphs.len(),
        new_paragraphs: new.paragraphs.len(),
        old_tables: old.tables.len(),
        new_tables: new.tables.len(),
        records,
        tables,
        summary,
        semantic: semantic_status,
        elapsed,
    })
}

fn prepare_semantic(
    config: &Config,
    old: &docx::Document,
    new: &docx::Document,
    progress: &mut dyn FnMut(Progress),
) -> Result<Option<(SemanticStatus, PreparedSemantic)>> {
    let Some(path) = config.model.path.as_ref() else {
        anyhow::bail!("未配置 [model].path");
    };
    if !path.exists() {
        anyhow::bail!("模型文件不存在：{}", path.display());
    }
    if let Some(tokenizer) = config.model.tokenizer_path.as_ref() {
        if !tokenizer.exists() {
            anyhow::bail!("tokenizer 文件不存在：{}", tokenizer.display());
        }
    }

    progress(Progress::Stage("加载语义模型".to_string()));
    let load_started = Instant::now();
    let mut embedder = Embedder::load(&config.model)?;
    log::info!("模型加载耗时 {:.2}s", load_started.elapsed().as_secs_f64());

    let old_rows: Vec<Vec<String>> = old
        .tables
        .iter()
        .map(matching::table::semantic_texts)
        .collect();
    let new_rows: Vec<Vec<String>> = new
        .tables
        .iter()
        .map(matching::table::semantic_texts)
        .collect();
    let old_row_count: usize = old_rows.iter().map(Vec::len).sum();
    let new_row_count: usize = new_rows.iter().map(Vec::len).sum();

    let mut unique: Vec<&str> = Vec::new();
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for paragraph in old.paragraphs.iter().chain(new.paragraphs.iter()) {
        collect_unique(&mut unique, &mut index_of, paragraph.normalized.as_str());
    }
    for texts in old_rows.iter().chain(new_rows.iter()) {
        for text in texts {
            collect_unique(&mut unique, &mut index_of, text);
        }
    }

    if unique.is_empty() {
        return Ok(None);
    }

    progress(Progress::Stage("计算语义向量".to_string()));
    progress(Progress::Step {
        label: "语义向量".to_string(),
        done: 0,
        total: unique.len(),
    });
    let mut all_vectors: Vec<Vec<f32>> = Vec::with_capacity(unique.len());
    let embed_started = Instant::now();
    let mut done = 0usize;
    for chunk in unique.chunks(EMBED_CHUNK) {
        let vectors = embedder.embed_texts(chunk)?;
        all_vectors.extend(vectors);
        done += chunk.len();
        progress(Progress::Step {
            label: "语义向量".to_string(),
            done,
            total: unique.len(),
        });
    }
    log::info!(
        "语义向量计算完成：{} 条唯一文本（段落 {} 条，表格行 {} 条），用时 {:.2}s",
        unique.len(),
        old.paragraphs.len() + new.paragraphs.len(),
        old_row_count + new_row_count,
        embed_started.elapsed().as_secs_f64()
    );

    let to_vectors = |paragraphs: &[crate::docx::Paragraph]| -> Vec<Vec<f32>> {
        paragraphs
            .iter()
            .map(|paragraph| all_vectors[index_of[paragraph.normalized.as_str()]].clone())
            .collect()
    };
    let to_table_vectors = |rows: &[Vec<String>]| -> Vec<Vec<Vec<f32>>> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|text| {
                        if text.is_empty() {
                            Vec::new()
                        } else {
                            all_vectors[index_of[text.as_str()]].clone()
                        }
                    })
                    .collect()
            })
            .collect()
    };

    let dim = embedder.dim();
    let status = SemanticStatus::enabled(Some(path.display().to_string()), Some(dim));
    log::info!("语义匹配已启用：{}，{} 维", path.display(), dim);
    Ok(Some((
        status,
        PreparedSemantic {
            paragraphs: SemanticVectors {
                old: to_vectors(&old.paragraphs),
                new: to_vectors(&new.paragraphs),
            },
            tables: TableVectors {
                old: to_table_vectors(&old_rows),
                new: to_table_vectors(&new_rows),
            },
        },
    )))
}

fn collect_unique<'a>(
    unique: &mut Vec<&'a str>,
    index_of: &mut HashMap<&'a str, usize>,
    text: &'a str,
) {
    if !index_of.contains_key(text) {
        index_of.insert(text, unique.len());
        unique.push(text);
    }
}

fn log_summary(records: &[crate::result::Record], elapsed: std::time::Duration) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    for record in records {
        log::debug!(
            "#{} {} 旧={:?} 新={:?} text={:.4} sem={:?} total={:.4}",
            record.id,
            record.status.as_str(),
            record.old_para_index,
            record.new_para_index,
            record.scores.text,
            record.scores.semantic,
            record.scores.total
        );
    }
    let modified = records
        .iter()
        .filter(|r| r.status == Status::Modified)
        .count();
    let suspected = records
        .iter()
        .filter(|r| r.status == Status::SuspectedModified)
        .count();
    log::debug!(
        "记录明细输出完毕：{} 条，修改 {}，疑似 {}，总耗时 {:.2}s",
        records.len(),
        modified,
        suspected,
        elapsed.as_secs_f64()
    );
}
