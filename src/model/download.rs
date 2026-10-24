use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// 与 BAAI/bge-small-zh-v1.5 对应的社区 ONNX 转换。
pub const REPO_ID: &str = "onnx-community/bge-small-zh-v1.5-ONNX";
pub const DEFAULT_ENDPOINT: &str = "https://huggingface.co";
pub const MIRROR_ENDPOINT: &str = "https://hf-mirror.com";

/// (仓库内路径, 本地文件名)
const FILES: [(&str, &str); 3] = [
    ("onnx/model.onnx", "model.onnx"),
    ("onnx/model.onnx_data", "model.onnx_data"),
    ("tokenizer.json", "tokenizer.json"),
];

/// 显式下载语义模型到 `directory`。仅此命令访问网络，正常比较流程不联网。
pub fn download_models(directory: &Path, endpoint: &str, force: bool) -> Result<()> {
    fs::create_dir_all(directory)
        .with_context(|| format!("创建模型目录失败：{}", directory.display()))?;
    let endpoint = endpoint.trim_end_matches('/');

    for (remote, name) in FILES {
        let destination = directory.join(name);
        if destination.exists() && !force {
            println!(
                "已存在，跳过：{}（使用 --force 覆盖）",
                destination.display()
            );
            continue;
        }
        let url = format!("{endpoint}/{REPO_ID}/resolve/main/{remote}");
        println!("下载 {url}");
        println!("  → {}", destination.display());
        download_file(&url, &destination)?;
    }

    println!("模型文件已就绪：{}", directory.display());
    println!("验证：diffract.exe --require-semantic --check 旧版.docx 新版.docx");
    Ok(())
}

fn download_file(url: &str, destination: &Path) -> Result<()> {
    let mut response = ureq::get(url)
        .header(
            "User-Agent",
            concat!("diffract/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .with_context(|| format!("下载失败：{url}"))?;
    let total = response
        .headers()
        .get("Content-Length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let part = part_path(destination);

    let result = (|| -> Result<()> {
        let file =
            File::create(&part).with_context(|| format!("创建临时文件失败：{}", part.display()))?;
        let mut writer = BufWriter::new(file);
        let mut reader = response.body_mut().as_reader();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut copied = 0u64;
        let mut next_report = 0u64;

        loop {
            let read = reader.read(&mut buffer).context("读取下载数据失败")?;
            if read == 0 {
                break;
            }
            writer
                .write_all(&buffer[..read])
                .with_context(|| format!("写入临时文件失败：{}", part.display()))?;
            copied += read as u64;
            if copied >= next_report || total.is_some_and(|total| copied >= total) {
                report_progress(destination, copied, total);
                next_report = copied + 2 * 1024 * 1024;
            }
        }
        writer
            .flush()
            .with_context(|| format!("刷新临时文件失败：{}", part.display()))?;
        drop(writer);

        if let Some(total) = total
            && copied != total
        {
            bail!("下载不完整：期望 {total} 字节，实际 {copied} 字节");
        }
        if destination.exists() {
            fs::remove_file(destination)
                .with_context(|| format!("覆盖旧模型文件失败：{}", destination.display()))?;
        }
        fs::rename(&part, destination).with_context(|| {
            format!(
                "保存模型文件失败：{} → {}",
                part.display(),
                destination.display()
            )
        })?;
        eprintln!();
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

fn report_progress(destination: &Path, copied: u64, total: Option<u64>) {
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("model");
    match total {
        Some(total) if total > 0 => eprint!(
            "\r  {name}: {}/{} ({:.0}%)",
            format_bytes(copied),
            format_bytes(total),
            copied as f64 / total as f64 * 100.0
        ),
        _ => eprint!("\r  {name}: {}", format_bytes(copied)),
    }
    let _ = std::io::stderr().flush();
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

fn part_path(destination: &Path) -> PathBuf {
    let mut value = destination.as_os_str().to_os_string();
    value.push(".part");
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_part_path_next_to_destination() {
        assert_eq!(
            part_path(Path::new("models/model.onnx_data")),
            PathBuf::from("models/model.onnx_data.part")
        );
    }

    #[test]
    fn endpoint_trailing_slash_is_trimmed() {
        let endpoint = "https://hf-mirror.com/";
        assert_eq!(endpoint.trim_end_matches('/'), "https://hf-mirror.com");
    }

    #[test]
    fn formats_small_and_large_progress_sizes() {
        assert_eq!(format_bytes(41_689), "40.7 KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MB");
    }
}
