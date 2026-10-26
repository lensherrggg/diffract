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

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use config::{Config, Overrides, Pooling};
use matching::Progress;
use model::download;

#[derive(Debug, Parser)]
#[command(
    name = "diffract",
    version,
    about = "diffRact：本地 .docx 段落与表格行级版本比较工具",
    long_about = "比较两个 .docx 的段落与表格行，识别未变、新增、删除、修改、移动、疑似修改六类差异。表格按行级记录，纵向合并单元格按内容展开到覆盖的每一行。默认进入 TUI；省略文件参数时先进入文件选择器；--check 或 --html/--md 用于 CI 和批处理。使用 download-model 子命令可显式下载语义模型（仅该命令联网）。",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,

    #[command(flatten)]
    compare: CompareArgs,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// 下载语义模型到本地（仅此命令访问网络）
    DownloadModel {
        /// 模型输出目录
        #[arg(long, value_name = "DIR", default_value = "models")]
        dir: PathBuf,

        /// 使用 hf-mirror.com 镜像
        #[arg(long)]
        mirror: bool,

        /// 覆盖已存在的模型文件
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, clap::Args)]
struct CompareArgs {
    /// 旧版本 .docx（TUI 模式下可省略）
    #[arg(value_name = "OLD")]
    old: Option<PathBuf>,

    /// 新版本 .docx（TUI 模式下可省略）
    #[arg(value_name = "NEW")]
    new: Option<PathBuf>,

    /// 非交互模式：输出统计并按 0/1/2 退出
    #[arg(long)]
    check: bool,

    /// 导出 HTML 报告到指定文件（隐含非交互模式）
    #[arg(long, value_name = "FILE")]
    html: Option<PathBuf>,

    /// 导出 Markdown 报告到指定文件（隐含非交互模式）
    #[arg(long, visible_alias = "markdown", value_name = "FILE")]
    md: Option<PathBuf>,

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
    if let Some(CliCommand::DownloadModel { dir, mirror, force }) = cli.command {
        let endpoint = if mirror {
            download::MIRROR_ENDPOINT
        } else {
            download::DEFAULT_ENDPOINT
        };
        download::download_models(&dir, endpoint, force)?;
        return Ok(ExitCode::SUCCESS);
    }

    let compare = cli.compare;
    let old = compare.old;
    let new = compare.new;
    let overrides = Overrides {
        window: compare.window,
        weight_text: compare.weight_text,
        weight_semantic: compare.weight_semantic,
        threshold_modified: compare.threshold_modified,
        threshold_suspected: compare.threshold_suspected,
        text_candidate_threshold: compare.text_candidate_threshold,
        model_path: compare.model,
        tokenizer_path: compare.tokenizer,
        max_seq_len: compare.max_seq_len,
        no_semantic: compare.no_semantic,
        require_semantic: compare.require_semantic,
        output_name: compare.output_name,
        pooling: compare.pooling,
        dim: compare.dim,
        context_chars: compare.context_chars,
        log_path: compare.log,
        log_level: compare.log_level,
    };
    let config = Config::load(compare.config.as_deref(), &overrides)?;
    logging::init(&config.log)?;
    match (&old, &new) {
        (Some(old), Some(new)) => {
            log::info!("diffract 启动：{} -> {}", old.display(), new.display());
        }
        _ => log::info!("diffract 启动：等待选择对比文件"),
    }

    if compare.check || compare.html.is_some() || compare.md.is_some() {
        let old = old.context("缺少旧版本 .docx（OLD）")?;
        let new = new.context("缺少新版本 .docx（NEW）")?;
        let mut progress = CheckProgress::new();
        let output =
            engine::compare_files(&old, &new, &config, &mut |update| progress.update(update));
        progress.finish();
        let output = match output {
            Ok(output) => output,
            Err(err) => {
                log::error!("比较失败：{err:#}");
                return Err(err);
            }
        };
        if let Some(path) = compare.html.as_deref() {
            report::export_html(&output, path)?;
        }
        if let Some(path) = compare.md.as_deref() {
            report::export_markdown(&output, path)?;
        }
        print!("{}", report::check_report(&output));
        if let Some(path) = compare.html.as_deref() {
            println!("HTML 报告：{}", path.display());
        }
        if let Some(path) = compare.md.as_deref() {
            println!("Markdown 报告：{}", path.display());
        }
        std::io::stdout().flush()?;
        Ok(if output.summary.has_differences() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    } else {
        if !std::io::stdout().is_terminal() {
            anyhow::bail!("TUI 需要交互式终端；请在 CI 或管道中使用 --check 或 --html/--md");
        }
        match (old, new) {
            (Some(old), Some(new)) => tui::run(old, new, config),
            (old, new) => tui::run_with_picker(old, new, config),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_flags_are_parsed() {
        let cli = Cli::try_parse_from([
            "diffract",
            "old.docx",
            "new.docx",
            "--html",
            "out/report.html",
            "--md",
            "out/report.md",
        ])
        .unwrap();
        assert_eq!(cli.compare.html, Some(PathBuf::from("out/report.html")));
        assert_eq!(cli.compare.md, Some(PathBuf::from("out/report.md")));
        assert!(!cli.compare.check);
    }

    #[test]
    fn markdown_alias_is_accepted() {
        let cli = Cli::try_parse_from([
            "diffract",
            "old.docx",
            "new.docx",
            "--markdown",
            "report.md",
        ])
        .unwrap();
        assert_eq!(cli.compare.md, Some(PathBuf::from("report.md")));
    }

    #[test]
    fn download_model_subcommand_is_parsed() {
        let cli = Cli::try_parse_from([
            "diffract",
            "download-model",
            "--mirror",
            "--dir",
            "D:\\models",
            "--force",
        ])
        .unwrap();
        match cli.command {
            Some(CliCommand::DownloadModel { dir, mirror, force }) => {
                assert_eq!(dir, PathBuf::from("D:\\models"));
                assert!(mirror);
                assert!(force);
            }
            None => panic!("download-model 子命令未被识别"),
        }
    }

    #[test]
    fn compare_args_do_not_require_paths_when_downloading() {
        let cli = Cli::try_parse_from(["diffract", "download-model"]).unwrap();
        assert!(cli.compare.old.is_none());
        assert!(cli.compare.new.is_none());
    }

    #[test]
    fn tui_mode_allows_a_missing_file_argument() {
        let cli = Cli::try_parse_from(["diffract"]).unwrap();
        assert!(cli.compare.old.is_none());
        assert!(cli.compare.new.is_none());

        let cli = Cli::try_parse_from(["diffract", "old.docx"]).unwrap();
        assert_eq!(cli.compare.old, Some(PathBuf::from("old.docx")));
        assert!(cli.compare.new.is_none());
    }
}
