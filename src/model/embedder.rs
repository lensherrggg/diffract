use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::config::{ModelConfig, Pooling};

struct ModelInner {
    session: Session,
    tokenizer: Tokenizer,
    config: ModelConfig,
    cls_id: Option<u32>,
    sep_id: Option<u32>,
    pad_id: u32,
    dim: usize,
    feed_token_type_ids: bool,
}

pub struct Embedder {
    inner: ModelInner,
    _environment: ort::environment::Environment,
}

fn environment() -> Result<&'static ort::environment::Environment> {
    static ENV: OnceLock<ort::environment::Environment> = OnceLock::new();
    if let Some(env) = ENV.get() {
        return Ok(env);
    }
    let env = ort::init()
        .with_name("diffract")
        .build()
        .map_err(|err| anyhow::anyhow!("初始化 ONNX Runtime 失败：{err}"))?;
    Ok(ENV.get_or_init(|| env))
}

impl Embedder {
    pub fn load(config: &ModelConfig) -> Result<Self> {
        let Some(model_path) = &config.path else {
            bail!("未配置 [model].path");
        };
        let Some(tokenizer_path) = &config.tokenizer_path else {
            bail!("未配置 [model].tokenizer_path");
        };
        if !model_path.exists() {
            bail!("模型文件不存在：{}", model_path.display());
        }
        if !tokenizer_path.exists() {
            bail!("tokenizer 文件不存在：{}", tokenizer_path.display());
        }

        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|err| anyhow::anyhow!("加载 tokenizer 失败：{err}"))?;
        let cls_id = tokenizer
            .token_to_id("[CLS]")
            .or_else(|| tokenizer.token_to_id("<s>"));
        let sep_id = tokenizer
            .token_to_id("[SEP]")
            .or_else(|| tokenizer.token_to_id("</s>"));
        let pad_id = tokenizer
            .token_to_id("[PAD]")
            .or_else(|| tokenizer.token_to_id("<pad>"))
            .or(Some(0))
            .unwrap();

        let threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 8))
            .unwrap_or(4);
        let env = environment()?;
        let session = Session::builder(env)
            .map_err(|err| anyhow::anyhow!("创建 ONNX 会话失败：{err}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|err| anyhow::anyhow!("设置图优化级别失败：{err}"))?
            .with_intra_threads(threads)
            .map_err(|err| anyhow::anyhow!("设置推理线程失败：{err}"))?
            .commit_from_file(model_path)
            .with_context(|| format!("加载 ONNX 模型失败：{}", model_path.display()))?;

        let dim = infer_dim(&session, config)?;
        let feed_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");
        log::info!(
            "语义模型加载成功：{}，dim={}，threads={}，token_type_ids={}",
            model_path.display(),
            dim,
            threads,
            feed_token_type_ids
        );

        Ok(Self {
            inner: ModelInner {
                session,
                tokenizer,
                config: config.clone(),
                cls_id,
                sep_id,
                pad_id,
                dim,
                feed_token_type_ids,
            },
            _environment: env.clone(),
        })
    }

    pub fn dim(&self) -> usize {
        self.inner.dim
    }

    /// 对一批文本做分块、批量推理，分块向量取平均后再归一化。
    pub fn embed_texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut chunks_per_text: Vec<Vec<Vec<u32>>> = Vec::with_capacity(texts.len());
        for text in texts {
            let encoding = self
                .inner
                .tokenizer
                .encode(*text, false)
                .map_err(|err| anyhow::anyhow!("tokenizer 编码失败：{err}"))?;
            chunks_per_text.push(self.split_chunks(encoding.get_ids())?);
            log::debug!(
                "tokenizer 编码 {} 个字符得到 {} 个 token，首批 id {:?}",
                text.chars().count(),
                encoding.get_ids().len(),
                &encoding.get_ids()[..encoding.get_ids().len().min(12)]
            );
        }

        let batch_size = self.inner.config.batch_size.max(1);
        let mut sums: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        let mut counts: Vec<usize> = vec![0; texts.len()];
        let mut pending: Vec<(usize, Vec<u32>)> = Vec::new();
        for (text_index, chunks) in chunks_per_text.into_iter().enumerate() {
            for chunk in chunks {
                pending.push((text_index, chunk));
            }
        }

        for batch in pending.chunks(batch_size) {
            let max_len = batch.iter().map(|(_, ids)| ids.len()).max().unwrap_or(0);
            if max_len == 0 {
                continue;
            }
            let mut input_ids = Vec::with_capacity(batch.len() * max_len);
            let mut attention_mask = Vec::with_capacity(batch.len() * max_len);
            for (_, ids) in batch {
                for index in 0..max_len {
                    if let Some(id) = ids.get(index) {
                        input_ids.push(i64::from(*id));
                        attention_mask.push(1i64);
                    } else {
                        input_ids.push(i64::from(self.inner.pad_id));
                        attention_mask.push(0i64);
                    }
                }
            }

            let batch_len = batch.len();
            let pooling = self.inner.config.pooling;
            let expected_dim = self.inner.dim;
            let attention_mask_for_pooling = attention_mask.clone();
            let input_ids = Tensor::from_array(([batch_len, max_len], input_ids))
                .map_err(|err| anyhow::anyhow!("构造 input_ids 张量失败：{err}"))?;
            let attention_mask_tensor = Tensor::from_array(([batch_len, max_len], attention_mask))
                .map_err(|err| anyhow::anyhow!("构造 attention_mask 张量失败：{err}"))?;

            let token_type_ids = if self.inner.feed_token_type_ids {
                Some(
                    Tensor::from_array(([batch_len, max_len], vec![0i64; batch_len * max_len]))
                        .map_err(|err| anyhow::anyhow!("构造 token_type_ids 张量失败：{err}"))?,
                )
            } else {
                None
            };

            let outputs = match token_type_ids {
                Some(token_type_ids) => self.inner.session.run(ort::inputs![
                    self.inner.config.input_ids_name.as_str() => input_ids,
                    self.inner.config.attention_mask_name.as_str() => attention_mask_tensor,
                    "token_type_ids" => token_type_ids
                ]),
                None => self.inner.session.run(ort::inputs![
                    self.inner.config.input_ids_name.as_str() => input_ids,
                    self.inner.config.attention_mask_name.as_str() => attention_mask_tensor
                ]),
            }
            .map_err(|err| anyhow::anyhow!("ONNX 推理失败：{err}"))?;

            let output = outputs.get(&self.inner.config.output_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "模型输出中没有 [model].output_name = \"{}\"",
                    self.inner.config.output_name
                )
            })?;
            let (shape, data) = output
                .try_extract_tensor::<f32>()
                .map_err(|err| anyhow::anyhow!("读取模型输出失败（需要 float32 张量）：{err}"))?;
            let dims: Vec<usize> = shape.iter().map(|d| (*d).max(0) as usize).collect();

            for (batch_index, (text_index, _)) in batch.iter().enumerate() {
                let mask =
                    &attention_mask_for_pooling[batch_index * max_len..(batch_index + 1) * max_len];
                let vector = extract_vector(&dims, data, batch_index, mask, pooling, expected_dim)?;
                if vector.len() != self.inner.dim {
                    bail!(
                        "模型输出维度为 {}，与 [model].dim = {} 不一致",
                        vector.len(),
                        self.inner.dim
                    );
                }
                let sum = &mut sums[*text_index];
                if sum.is_empty() {
                    *sum = vec![0.0; vector.len()];
                }
                for (target, value) in sum.iter_mut().zip(vector.iter()) {
                    *target += value;
                }
                counts[*text_index] += 1;
            }
        }

        let mut result = Vec::with_capacity(texts.len());
        for (mut sum, count) in sums.into_iter().zip(counts) {
            if sum.is_empty() {
                sum = vec![0.0; self.inner.dim];
            } else if count > 1 {
                let divisor = count as f32;
                for value in &mut sum {
                    *value /= divisor;
                }
            }
            normalize_l2(&mut sum);
            result.push(sum);
        }
        Ok(result)
    }

    fn split_chunks(&self, ids: &[u32]) -> Result<Vec<Vec<u32>>> {
        let specials =
            usize::from(self.inner.cls_id.is_some()) + usize::from(self.inner.sep_id.is_some());
        let max_content = self
            .inner
            .config
            .max_seq_len
            .checked_sub(specials)
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "[model].max_seq_len = {} 太小，容纳不下特殊 token",
                    self.inner.config.max_seq_len
                )
            })?;

        let mut chunks = Vec::new();
        if ids.is_empty() {
            chunks.push(self.with_specials(&[]));
        } else {
            for chunk in ids.chunks(max_content) {
                chunks.push(self.with_specials(chunk));
            }
        }
        Ok(chunks)
    }

    fn with_specials(&self, content: &[u32]) -> Vec<u32> {
        let mut ids = Vec::with_capacity(content.len() + 2);
        if let Some(cls) = self.inner.cls_id {
            ids.push(cls);
        }
        ids.extend_from_slice(content);
        if let Some(sep) = self.inner.sep_id {
            ids.push(sep);
        }
        ids
    }
}

fn extract_vector(
    dims: &[usize],
    data: &[f32],
    batch_index: usize,
    attention: &[i64],
    pooling: Pooling,
    expected_dim: usize,
) -> Result<Vec<f32>> {
    match dims {
        [batch, dim] => {
            let _ = batch;
            if *dim != expected_dim {
                bail!(
                    "模型输出维度为 {}，与 [model].dim = {} 不一致",
                    dim,
                    expected_dim
                );
            }
            let start = batch_index * dim;
            Ok(data[start..start + dim].to_vec())
        }
        [batch, seq, dim] => {
            let _ = batch;
            if *dim != expected_dim {
                bail!(
                    "模型输出维度为 {}，与 [model].dim = {} 不一致",
                    dim,
                    expected_dim
                );
            }
            let base = batch_index * seq * dim;
            let vectors = &data[base..base + seq * dim];
            match pooling {
                Pooling::Cls => Ok(vectors[..*dim].to_vec()),
                Pooling::Mean => {
                    let mut sum = vec![0.0f32; *dim];
                    let mut count = 0usize;
                    for position in 0..*seq {
                        if attention.get(position).copied().unwrap_or(1) == 0 {
                            continue;
                        }
                        let offset = position * dim;
                        for (target, value) in sum.iter_mut().zip(&vectors[offset..offset + dim]) {
                            *target += value;
                        }
                        count += 1;
                    }
                    if count > 0 {
                        for value in &mut sum {
                            *value /= count as f32;
                        }
                    }
                    Ok(sum)
                }
                Pooling::Model => {
                    bail!("[model].pooling = \"model\" 要求输出为 [batch, dim]，实际是三维张量")
                }
            }
        }
        _ => bail!(
            "模型输出维度不合法：{:?}，应为 [batch, dim] 或 [batch, seq, dim]",
            dims
        ),
    }
}

fn normalize_l2(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value = (f64::from(*value) / norm) as f32;
        }
    }
}

fn infer_dim(session: &Session, config: &ModelConfig) -> Result<usize> {
    let configured = config.dim;
    if configured > 0 {
        return Ok(configured);
    }
    let output = session
        .outputs()
        .iter()
        .find(|output| output.name() == config.output_name)
        .or_else(|| session.outputs().first());
    let Some(output) = output else {
        bail!("模型没有任何输出");
    };
    let dims = output.dtype().tensor_shape();
    let Some(dims) = dims else {
        bail!("无法推断模型输出维度，请在 [model].dim 中显式配置");
    };
    let last = dims.last().copied().unwrap_or(0);
    if last <= 0 {
        bail!("无法从模型输出 {:?} 推断维度，请显式配置 [model].dim", dims);
    }
    log::info!("[model].dim 未配置，按模型输出推断为 {last}");
    Ok(last as usize)
}
