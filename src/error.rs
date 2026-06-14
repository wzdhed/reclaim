use std::fmt;
use std::io;
use std::path::PathBuf;

/// 扫描过程中可能发生的错误。
///
/// 单个文件/目录的错误会被收集进 `ScanResult.warnings`，不中断整次扫描。
#[derive(Debug)]
pub enum ScanError {
    /// 读取某个路径时发生 I/O 错误（如无权限、坏软链）。
    Io { path: PathBuf, source: io::Error },
    /// 终端/TUI 相关的 I/O 错误（raw mode、备用屏幕、渲染等）。
    Tui(io::Error),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            ScanError::Tui(source) => write!(f, "终端错误: {source}"),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ScanError::Io { source, .. } => Some(source),
            ScanError::Tui(source) => Some(source),
        }
    }
}

impl From<io::Error> for ScanError {
    fn from(source: io::Error) -> Self {
        ScanError::Tui(source)
    }
}

/// 库内统一使用的 `Result` 别名。
pub type Result<T> = std::result::Result<T, ScanError>;
