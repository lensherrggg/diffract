pub mod docx;

use std::path::Path;

use anyhow::{Result, bail};

use crate::config::Config;
use crate::matching::Progress;
use crate::result::CompareOutput;
use crate::xlsx::{self, XlsxOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Docx,
    Xlsx,
}

impl DocumentKind {
    pub fn from_path(path: &Path) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "docx" => Ok(Self::Docx),
            "xlsx" | "xlsm" => Ok(Self::Xlsx),
            "xls" => bail!("暂不支持 .xls：{}，请先另存为 .xlsx", path.display()),
            "" => bail!("无法识别文件类型：{}", path.display()),
            _ => bail!(
                "不支持的文件类型 .{extension}：{}（仅支持 .docx/.xlsx/.xlsm）",
                path.display()
            ),
        }
    }

    pub fn detect(old: &Path, new: &Path) -> Result<Self> {
        let old_kind = Self::from_path(old)?;
        let new_kind = Self::from_path(new)?;
        if old_kind != new_kind {
            bail!(
                "两个文件类型不一致：旧文件是 {}，新文件是 {}",
                old_kind.label_zh(),
                new_kind.label_zh()
            );
        }
        Ok(old_kind)
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Docx => "Word 文档",
            Self::Xlsx => "Excel 工作簿",
        }
    }

    pub fn extensions_label(self) -> &'static str {
        match self {
            Self::Docx => ".docx",
            Self::Xlsx => ".xlsx / .xlsm",
        }
    }

    pub fn accepts_name(self, name: &str) -> bool {
        let extension = name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();
        match self {
            Self::Docx => extension == "docx",
            Self::Xlsx => extension == "xlsx" || extension == "xlsm",
        }
    }
}

pub enum Output {
    Docx(CompareOutput),
    Xlsx(XlsxOutput),
}

pub fn compare(
    kind: DocumentKind,
    old: &Path,
    new: &Path,
    config: &Config,
    progress: &mut dyn FnMut(Progress),
) -> Result<Output> {
    match kind {
        DocumentKind::Docx => docx::compare(old, new, config, progress).map(Output::Docx),
        DocumentKind::Xlsx => xlsx::compare_files(old, new, config, progress).map(Output::Xlsx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_kinds() {
        assert_eq!(
            DocumentKind::from_path(Path::new("旧版.docx")).unwrap(),
            DocumentKind::Docx
        );
        assert_eq!(
            DocumentKind::from_path(Path::new("台账.XLSX")).unwrap(),
            DocumentKind::Xlsx
        );
        assert_eq!(
            DocumentKind::from_path(Path::new("台账.xlsm")).unwrap(),
            DocumentKind::Xlsx
        );
    }

    #[test]
    fn rejects_mixed_or_unsupported_files() {
        assert!(DocumentKind::detect(Path::new("a.docx"), Path::new("b.xlsx")).is_err());
        assert!(DocumentKind::from_path(Path::new("a.xls")).is_err());
        assert!(DocumentKind::from_path(Path::new("a.txt")).is_err());
    }

    #[test]
    fn name_filter_matches_the_engine() {
        let docx = DocumentKind::Docx;
        assert!(docx.accepts_name("报告.docx"));
        assert!(!docx.accepts_name("台账.xlsx"));
        let xlsx = DocumentKind::Xlsx;
        assert!(xlsx.accepts_name("台账.xlsx"));
        assert!(xlsx.accepts_name("台账.XLSM"));
        assert!(!xlsx.accepts_name("报告.docx"));
    }
}
