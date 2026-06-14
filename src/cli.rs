use crate::actions::DeleteMode;
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// 分析磁盘占用并查找重复文件。
#[derive(Parser)]
#[command(name = "reclaim", about = "分析磁盘占用并查找重复文件")]
pub struct Args {
    /// 要扫描的目录
    pub path: PathBuf,

    /// 只显示最大的 N 项
    #[arg(long, default_value_t = 20)]
    pub top: usize,

    /// 忽略小于该大小的文件，如 1M、500K
    #[arg(long, default_value = "0", value_parser = parse_size)]
    pub min_size: u64,

    /// 启用重复文件检测
    #[arg(long)]
    pub dup: bool,

    /// 输出格式
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,

    /// 工作线程数（默认 = CPU 核数）
    #[arg(long)]
    pub threads: Option<usize>,

    /// 包含隐藏文件（默认忽略）
    #[arg(long)]
    pub hidden: bool,

    /// 启动交互式 TUI 界面
    #[arg(long)]
    pub tui: bool,

    /// 删除方式：trash（移入回收区，可恢复）/ permanent（永久删除）
    #[arg(long, value_enum, default_value_t = DeleteMode::Trash)]
    pub delete_mode: DeleteMode,
}

/// 输出格式。
#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Table,
    Tree,
    Json,
}

/// 把人类可读的大小（如 `1M`、`500K`）解析成字节数。整数即字节，可带 K/M/G/T（大小写不敏感，可选 B 后缀）。
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("大小不能为空".to_string());
    }

    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num_part, suffix) = s.split_at(digits_end);
    if num_part.is_empty() {
        return Err(format!("无效的大小: {s}"));
    }
    let num: u64 = num_part.parse().map_err(|_| format!("无效的大小: {s}"))?;

    let mut suffix = suffix.trim().to_ascii_uppercase();
    if suffix.ends_with('B') {
        suffix.pop();
    }
    let mult: u64 = match suffix.as_str() {
        "" => 1,
        "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("未知的大小单位: {s}")),
    };

    num.checked_mul(mult)
        .ok_or_else(|| format!("大小溢出: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("500"), Ok(500));
        assert_eq!(parse_size("1K"), Ok(1024));
        assert_eq!(parse_size("500K"), Ok(500 * 1024));
        assert_eq!(parse_size("1M"), Ok(1024 * 1024));
        assert_eq!(parse_size("2G"), Ok(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1mb"), Ok(1024 * 1024)); // 大小写不敏感 + B 后缀
    }

    #[test]
    fn parse_size_rejects_invalid() {
        assert!(parse_size("abc").is_err());
        assert!(parse_size("1X").is_err());
        assert!(parse_size("").is_err());
    }
}
