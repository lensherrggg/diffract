use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub similarity: SimilarityConfig,
    pub model: ModelConfig,
    pub diff: DiffConfig,
    pub log: LogConfig,
    pub xlsx: XlsxConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            similarity: SimilarityConfig::default(),
            model: ModelConfig::default(),
            diff: DiffConfig::default(),
            log: LogConfig::default(),
            xlsx: XlsxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum XlsxCompareMode {
    #[default]
    Value,
    Formula,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct XlsxConfig {
    /// 1 基表头行。
    /// `None` 自动识别；`Some(0)` 无表头；`Some(n)` 固定第 n 行。
    pub header_row: Option<usize>,
    /// 1 基行键列，例如 [1] 表示 A 列；空表示按整行内容对齐。
    pub key_columns: Vec<usize>,
    pub compare_mode: XlsxCompareMode,
    pub row_window: usize,
    pub col_window: usize,
    /// 预留：首版 Excel 比较不加载语义模型。
    pub semantic_enabled: bool,
    pub max_iterations: usize,
    pub ignore_empty_rows: bool,
}

impl Default for XlsxConfig {
    fn default() -> Self {
        Self {
            header_row: None,
            key_columns: Vec::new(),
            compare_mode: XlsxCompareMode::Value,
            row_window: 10,
            col_window: 3,
            semantic_enabled: false,
            max_iterations: 5,
            ignore_empty_rows: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SimilarityConfig {
    pub window: usize,
    pub weight_text: f64,
    pub weight_semantic: f64,
    pub threshold_modified: f64,
    pub threshold_suspected: f64,
    pub text_candidate_threshold: f64,
}

impl Default for SimilarityConfig {
    fn default() -> Self {
        Self {
            window: 5,
            weight_text: 0.6,
            weight_semantic: 0.4,
            threshold_modified: 0.75,
            threshold_suspected: 0.45,
            text_candidate_threshold: 0.30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum Pooling {
    #[default]
    Mean,
    Cls,
    /// 模型自身已输出 [batch, dim] 池化向量。
    Model,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub path: Option<PathBuf>,
    pub tokenizer_path: Option<PathBuf>,
    pub max_seq_len: usize,
    pub enabled: bool,
    /// 为 true 时模型加载失败将直接报错（退出码 2），而不是退化。
    pub required: bool,
    pub output_name: String,
    pub pooling: Pooling,
    pub dim: usize,
    pub batch_size: usize,
    pub input_ids_name: String,
    pub attention_mask_name: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            path: Some(PathBuf::from("models/model.onnx")),
            tokenizer_path: Some(PathBuf::from("models/tokenizer.json")),
            max_seq_len: 512,
            enabled: true,
            required: false,
            output_name: "last_hidden_state".to_string(),
            pooling: Pooling::Mean,
            dim: 768,
            batch_size: 8,
            input_ids_name: "input_ids".to_string(),
            attention_mask_name: "attention_mask".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiffConfig {
    pub context_chars: usize,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self { context_chars: 80 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub level: String,
    pub path: Option<PathBuf>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            path: None,
        }
    }
}

/// CLI 参数覆盖项。`None` 表示不覆盖配置文件。
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub window: Option<usize>,
    pub weight_text: Option<f64>,
    pub weight_semantic: Option<f64>,
    pub threshold_modified: Option<f64>,
    pub threshold_suspected: Option<f64>,
    pub text_candidate_threshold: Option<f64>,
    pub model_path: Option<PathBuf>,
    pub tokenizer_path: Option<PathBuf>,
    pub max_seq_len: Option<usize>,
    pub no_semantic: bool,
    pub require_semantic: bool,
    pub output_name: Option<String>,
    pub pooling: Option<Pooling>,
    pub dim: Option<usize>,
    pub context_chars: Option<usize>,
    pub log_path: Option<PathBuf>,
    pub log_level: Option<String>,
}

fn read_config_file(path: &Path) -> Result<(Config, PathBuf)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("配置文件格式错误：{}", path.display()))?;
    let base = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    Ok((config, base))
}

/// 当前目录优先，其次回退到 exe 所在目录，便于把程序加入 PATH 后从任意目录启动。
fn default_config_path() -> Option<PathBuf> {
    let local = PathBuf::from("diffract.toml");
    let bundled = std::env::current_exe()
        .ok()?
        .parent()?
        .join("diffract.toml");
    pick_config(&local, &bundled)
}

fn pick_config(local: &Path, bundled: &Path) -> Option<PathBuf> {
    if local.is_file() {
        Some(local.to_path_buf())
    } else if bundled.is_file() {
        Some(bundled.to_path_buf())
    } else {
        None
    }
}

impl Config {
    pub fn load(explicit_path: Option<&Path>, overrides: &Overrides) -> Result<Self> {
        let (mut config, base_dir) = match explicit_path {
            Some(path) => {
                if !path.exists() {
                    bail!("配置文件不存在：{}", path.display());
                }
                read_config_file(path)?
            }
            None => match default_config_path() {
                Some(path) => read_config_file(&path)?,
                None => (Config::default(), PathBuf::new()),
            },
        };

        config.resolve_relative_paths(&base_dir);
        config.apply_overrides(overrides);
        config.normalize_paths();
        config.validate()?;
        Ok(config)
    }

    fn resolve_relative_paths(&mut self, base: &Path) {
        if base.as_os_str().is_empty() {
            return;
        }
        for path in [
            &mut self.model.path,
            &mut self.model.tokenizer_path,
            &mut self.log.path,
        ] {
            if let Some(value) = path {
                if value.is_relative() {
                    *value = base.join(&*value);
                }
            }
        }
    }

    pub fn apply_overrides(&mut self, overrides: &Overrides) {
        let s = &mut self.similarity;
        if let Some(v) = overrides.window {
            s.window = v;
        }
        if let Some(v) = overrides.weight_text {
            s.weight_text = v;
        }
        if let Some(v) = overrides.weight_semantic {
            s.weight_semantic = v;
        }
        if let Some(v) = overrides.threshold_modified {
            s.threshold_modified = v;
        }
        if let Some(v) = overrides.threshold_suspected {
            s.threshold_suspected = v;
        }
        if let Some(v) = overrides.text_candidate_threshold {
            s.text_candidate_threshold = v;
        }

        let m = &mut self.model;
        if let Some(v) = &overrides.model_path {
            m.path = Some(v.clone());
        }
        if let Some(v) = &overrides.tokenizer_path {
            m.tokenizer_path = Some(v.clone());
        }
        if let Some(v) = overrides.max_seq_len {
            m.max_seq_len = v;
        }
        if overrides.no_semantic {
            m.enabled = false;
            m.required = false;
        }
        if overrides.require_semantic {
            m.enabled = true;
            m.required = true;
        }
        if let Some(v) = &overrides.output_name {
            m.output_name = v.clone();
        }
        if let Some(v) = overrides.pooling {
            m.pooling = v;
        }
        if let Some(v) = overrides.dim {
            m.dim = v;
        }

        if let Some(v) = overrides.context_chars {
            self.diff.context_chars = v;
        }
        if let Some(v) = &overrides.log_path {
            self.log.path = Some(v.clone());
        }
        if let Some(v) = &overrides.log_level {
            self.log.level = v.clone();
        }
    }

    fn normalize_paths(&mut self) {
        if self
            .model
            .path
            .as_ref()
            .is_some_and(|p| p.as_os_str().is_empty())
        {
            self.model.path = None;
        }
        if self
            .model
            .tokenizer_path
            .as_ref()
            .is_some_and(|p| p.as_os_str().is_empty())
        {
            self.model.tokenizer_path = None;
        }
        if self
            .log
            .path
            .as_ref()
            .is_some_and(|p| p.as_os_str().is_empty())
        {
            self.log.path = None;
        }
    }

    pub fn validate(&self) -> Result<()> {
        let s = &self.similarity;
        if s.window == 0 {
            bail!("配置非法：[similarity].window 必须大于 0");
        }
        for (name, value) in [
            ("weight_text", s.weight_text),
            ("weight_semantic", s.weight_semantic),
        ] {
            if !value.is_finite() || value < 0.0 {
                bail!("配置非法：[similarity].{name} 必须是非负有限数");
            }
        }
        if s.weight_text + s.weight_semantic <= 0.0 {
            bail!("配置非法：[similarity] 的文本/语义权重不能同时为 0");
        }
        for (name, value) in [
            ("threshold_modified", s.threshold_modified),
            ("threshold_suspected", s.threshold_suspected),
            ("text_candidate_threshold", s.text_candidate_threshold),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                bail!("配置非法：[similarity].{name} 必须在 0 到 1 之间");
            }
        }
        if s.threshold_suspected > s.threshold_modified {
            bail!(
                "配置非法：threshold_suspected（{}）不能大于 threshold_modified（{}）",
                s.threshold_suspected,
                s.threshold_modified
            );
        }
        if self.diff.context_chars == 0 {
            bail!("配置非法：[diff].context_chars 必须大于 0");
        }
        if self.model.enabled {
            if self.model.max_seq_len < 8 {
                bail!("配置非法：[model].max_seq_len 不能小于 8");
            }
            if self.model.batch_size == 0 {
                bail!("配置非法：[model].batch_size 必须大于 0");
            }
            if self.model.required && self.model.path.is_none() {
                bail!("配置非法：要求必须启用语义匹配，但未配置 [model].path");
            }
            if self.model.output_name.trim().is_empty() {
                bail!("配置非法：[model].output_name 不能为空");
            }
            if self.model.input_ids_name.trim().is_empty()
                || self.model.attention_mask_name.trim().is_empty()
            {
                bail!("配置非法：[model] 的输入名不能为空");
            }
        }
        if self.model.required && !self.model.enabled {
            bail!("配置非法：[model].required 为 true 时必须同时 enabled = true");
        }
        if !matches!(
            self.log.level.to_ascii_lowercase().as_str(),
            "off" | "error" | "warn" | "info" | "debug" | "trace"
        ) {
            bail!(
                "配置非法：[log].level 只能是 off/error/warn/info/debug/trace，当前为 {}",
                self.log.level
            );
        }
        if self.xlsx.row_window == 0 {
            bail!("配置非法：[xlsx].row_window 必须大于 0");
        }
        if self.xlsx.col_window == 0 {
            bail!("配置非法：[xlsx].col_window 必须大于 0");
        }
        if self.xlsx.max_iterations == 0 {
            bail!("配置非法：[xlsx].max_iterations 必须大于 0");
        }
        if self.xlsx.key_columns.contains(&0) {
            bail!("配置非法：[xlsx].key_columns 使用 1 基列号，不能包含 0");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("config-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn prefers_working_directory_config() {
        let root = temp_root("prefer-cwd");
        let cwd = root.join("cwd");
        let exe = root.join("exe");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&exe).unwrap();
        fs::write(cwd.join("diffract.toml"), "[diff]\ncontext_chars = 12\n").unwrap();
        fs::write(exe.join("diffract.toml"), "[diff]\ncontext_chars = 34\n").unwrap();

        let picked = pick_config(&cwd.join("diffract.toml"), &exe.join("diffract.toml"));
        assert_eq!(picked, Some(cwd.join("diffract.toml")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn falls_back_to_exe_directory_config() {
        let root = temp_root("fallback-exe");
        let cwd = root.join("cwd");
        let exe = root.join("exe");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&exe).unwrap();
        let bundled = exe.join("diffract.toml");
        fs::write(&bundled, "[diff]\ncontext_chars = 34\n").unwrap();

        let picked = pick_config(&cwd.join("diffract.toml"), &bundled);
        assert_eq!(picked, Some(bundled.clone()));

        let (mut config, base) = read_config_file(&bundled).unwrap();
        assert_eq!(base, exe);
        config.resolve_relative_paths(&base);
        assert_eq!(
            config.model.path,
            Some(exe.join("models").join("model.onnx"))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn returns_none_without_any_config() {
        let root = temp_root("no-config");
        let cwd = root.join("cwd");
        let exe = root.join("exe");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&exe).unwrap();

        assert_eq!(
            pick_config(&cwd.join("diffract.toml"), &exe.join("diffract.toml")),
            None
        );
        let _ = fs::remove_dir_all(root);
    }
}
