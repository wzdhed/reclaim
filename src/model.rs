use crate::error::ScanError;
use std::path::PathBuf;

/// 一个目录项的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// 扫描到的单个条目。
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub kind: EntryKind,
}

/// 一次扫描的完整结果。
#[derive(Debug)]
pub struct ScanResult {
    pub root: PathBuf,
    pub entries: Vec<FileEntry>,
    pub total_size: u64,
    /// 单文件错误收集于此，不中断扫描。
    pub warnings: Vec<ScanError>,
}

/// 一组内容完全相同的重复文件。
#[derive(Debug)]
pub struct DuplicateGroup {
    /// blake3 全量哈希。
    pub hash: [u8; 32],
    pub size: u64,
    pub paths: Vec<PathBuf>,
}
