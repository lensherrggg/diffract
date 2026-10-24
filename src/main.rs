mod config;
mod diff;
mod docx;
mod engine;
mod logging;
mod matching;
mod model;
mod normalize;
mod report;
mod result;
mod tui;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use config::{Config, Overrides, Pooling};
use matching::Progress;

#[derive(Debug, Parser)]
#[command(
    name = "diffract",
    version,
    about = "diffRact：本地 .docx 段落与表格行级版本比较工具",
    long_about = "比较两个 .docx 的段落与表格行，识别未变、新增、删除、修改、移动、疑似修改六类差异。表格按行级记录，纵向合并单元格按内容展开到覆盖的每一行。默认进入 TUI；--check 用于 CI。"
)]
struct Cli {
    /// 旧版本 .docx
    #[arg(value_name = "OLD")]
    old: PathBuf,

    /// 新版本 .docx
    #[arg(value_name = "NEW")]
    new: PathBuf,

    /// 非交互模式：输出统计并按 0/1/2 退出
    #[arg(long)]
    check: bool,

    /// 指定 TOML 配置文件（默认读取工作目录下的 diffract.toml）
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// 匹配窗口大小
    #[arg(long, value_name = "N")]
    window: Option<usize>,

    /// 文本相似度权重
    #[arg(long, value_name = "F")]
    weight_text: Option<f64>,

    /// 语义相似度权重
    #[arg(long, value_name = "F")]
    weight_semantic: Option<f64>,

    /// 确认修改阈值
    #[arg(long, value_name = "F")]
    threshold_modified: Option<f64>,

    /// 疑似修改阈值
    #[arg(long, value_name = "F")]
    threshold_suspected: Option<f64>,

    /// 文本相似度候选阈值
    #[arg(long, value_name = "F")]
    text_candidate_threshold: Option<f64>,

    /// 外置 ONNX 模型路径
    #[arg(long, value_name = "FILE")]
    model: Option<PathBuf>,

    /// Hugging Face tokenizer.json 路径
    #[arg(long, value_name = "FILE")]
    tokenizer: Option<PathBuf>,

    /// 模型最大序列长度
    #[arg(long, value_name = "N")]
    max_seq_len: Option<usize>,

    /// 关闭语义匹配
    #[arg(long, conflicts_with = "require_semantic")]
    no_semantic: bool,

    /// 要求语义匹配必须可用，失败时退出码 2
    #[arg(long)]
    require_semantic: bool,

    /// 模型输出名
    #[arg(long, value_name = "NAME")]
    output_name: Option<String>,

    /// 池化方式
    #[arg(long, value_enum, value_name = "POOLING")]
    pooling: Option<Pooling>,

    /// 向量维度（0 表示按模型推断）
    #[arg(long, value_name = "N")]
    dim: Option<usize>,

    /// 段内 diff 展示上下文长度
    #[arg(long, value_name = "N")]
    context_chars: Option<usize>,

    /// 日志文件路径（不写 stderr，避免污染界面）
    #[arg(long, value_name = "FILE")]
    log: Option<PathBuf>,

    /// 日志级别：off/error/warn/info/debug/trace
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("错误：{err:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let overrides = Overrides {
        window: cli.window,
        weight_text: cli.weight_text,
        weight_semantic: cli.weight_semantic,
        threshold_modified: cli.threshold_modified,
        threshold_suspected: cli.threshold_suspected,
        text_candidate_threshold: cli.text_candidate_threshold,
        model_path: cli.model,
        tokenizer_path: cli.tokenizer,
        max_seq_len: cli.max_seq_len,
        no_semantic: cli.no_semantic,
        require_semantic: cli.require_semantic,
        output_name: cli.output_name,
        pooling: cli.pooling,
        dim: cli.dim,
        context_chars: cli.context_chars,
        log_path: cli.log,
        log_level: cli.log_level,
    };
    let config = Config::load(cli.config.as_deref(), &overrides)?;
    logging::init(&config.log)?;
    log::info!(
        "diffract 启动：{} -> {}",
        cli.old.display(),
        cli.new.display()
    );

    if cli.check {
        let mut progress = CheckProgress::new();
        let output = engine::compare_files(&cli.old, &cli.new, &config, &mut |update| {
            progress.update(update)
        });
        progress.finish();
        let output = match output {
            Ok(output) => output,
            Err(err) => {
                log::error!("比较失败：{err:#}");
                return Err(err);
            }
        };
        print!("{}", report::check_report(&output));
        std::io::stdout().flush()?;
        Ok(if output.summary.has_differences() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    } else {
        if !std::io::stdout().is_terminal() {
            anyhow::bail!("TUI 需要交互式终端；请在 CI 或管道中使用 --check");
        }
        tui::run(cli.old, cli.new, config)
    }
}

struct CheckProgress {
    tty: bool,
    width: usize,
}

impl CheckProgress {
    fn new() -> Self {
        Self {
            tty: std::io::stderr().is_terminal(),
            width: 0,
        }
    }

    fn update(&mut self, update: Progress) {
        if !self.tty {
            return;
        }
        match update {
            Progress::Stage(stage) => {
                if self.width > 0 {
                    eprintln!();
                    self.width = 0;
                }
                eprintln!("[diffract] {stage}");
            }
            Progress::Step { label, done, total } => {
                if total == 0 {
                    return;
                }
                let line = format!("[diffract] {label} {done}/{total}");
                let pad = self.width.saturating_sub(line.chars().count());
                eprint!("\r{line}{}", " ".repeat(pad));
                let _ = std::io::stderr().flush();
                self.width = line.chars().count();
            }
        }
    }

    fn finish(&mut self) {
        if self.tty && self.width > 0 {
            eprintln!();
        }
    }
}
