use crate::model::{DuplicateGroup, EntryKind, FileEntry};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 删除方式。
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DeleteMode {
    /// 移入 `<root>/.reclaim-trash/` 隔离区（默认，可恢复）。
    Trash,
    /// 永久删除（不可恢复）。
    Permanent,
}

/// 一次删除的预览（dry-run）：将删的文件清单 + 合计可释放空间。
///
/// 用于第 5 步的预览展示，不执行任何删除。
pub struct DeletePlan {
    pub targets: Vec<FileEntry>,
    pub reclaimable: u64,
}

impl DeletePlan {
    /// 从「已标记路径集合」与全部扫描条目构造预览：只纳入被标记的文件。
    pub fn build(marked: &HashSet<PathBuf>, entries: &[FileEntry]) -> Self {
        let targets: Vec<FileEntry> = entries
            .iter()
            .filter(|e| marked.contains(&e.path))
            .cloned()
            .collect();
        let reclaimable = targets.iter().map(|e| e.size).sum();
        Self {
            targets,
            reclaimable,
        }
    }
}

/// 经过安全保护后真正会被删除的集合。
pub struct SafeDeletion {
    /// 将被删除的文件。
    pub targets: Vec<FileEntry>,
    /// 为「每个重复组至少保留一份」而从删除集中保留下来的路径。
    pub kept_for_safety: Vec<PathBuf>,
}

/// 对标记集合套用全部安全保护，算出真正可删的文件（**纯函数，不写盘**）。
///
/// 保护：① 只删 `root` 之内的**文件**（拒目录、拒根外）；
/// ② 某重复组若被整组标记，强制保留第一份（绝不把一组删光）。
pub fn plan_safe_deletion(
    marked: &HashSet<PathBuf>,
    entries: &[FileEntry],
    dups: &[DuplicateGroup],
    root: &Path,
) -> SafeDeletion {
    // 初始删除集：被标记、在 root 之内的普通文件。
    let mut to_delete: HashSet<PathBuf> = entries
        .iter()
        .filter(|e| {
            e.kind == EntryKind::File && marked.contains(&e.path) && e.path.starts_with(root)
        })
        .map(|e| e.path.clone())
        .collect();

    // 重复组保护：整组都在删除集里时，保留第一份。
    let mut kept_for_safety = Vec::new();
    for group in dups {
        if group.paths.len() >= 2
            && group.paths.iter().all(|p| to_delete.contains(p))
            && let Some(keep) = group.paths.first()
        {
            to_delete.remove(keep);
            kept_for_safety.push(keep.clone());
        }
    }

    let targets: Vec<FileEntry> = entries
        .iter()
        .filter(|e| to_delete.contains(&e.path))
        .cloned()
        .collect();

    SafeDeletion {
        targets,
        kept_for_safety,
    }
}

/// 一次删除执行的结果。
pub struct DeleteOutcome {
    pub deleted: Vec<PathBuf>,
    pub freed: u64,
    /// 删除失败的项：路径 + 原因。
    pub failed: Vec<(PathBuf, String)>,
}

/// 真正执行删除（**这是全工具唯一写磁盘的地方**）。
///
/// 逐个文件：执行前再校验在 root 之内且仍是普通文件；`Trash` 移入隔离区、`Permanent` 直接删；
/// 成功则追加审计日志；单个失败收进 `failed` 不中断其余。
pub fn execute_deletion(targets: &[FileEntry], mode: DeleteMode, root: &Path) -> DeleteOutcome {
    let mut outcome = DeleteOutcome {
        deleted: Vec::new(),
        freed: 0,
        failed: Vec::new(),
    };
    let trash_root = root.join(".reclaim-trash");
    let log_path = root.join("reclaim-deleted.log");

    for target in targets {
        let path = &target.path;

        // 越界保护：执行前再确认在 root 之内。
        if !path.starts_with(root) {
            outcome
                .failed
                .push((path.clone(), "拒绝：路径在扫描根之外".to_string()));
            continue;
        }
        // 只删普通文件：执行前再校验（防 TOCTOU 把目录/软链删掉）。
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                outcome
                    .failed
                    .push((path.clone(), "拒绝：不是普通文件".to_string()));
                continue;
            }
            Err(e) => {
                outcome.failed.push((path.clone(), e.to_string()));
                continue;
            }
        }

        let result = match mode {
            DeleteMode::Trash => move_to_trash(path, root, &trash_root),
            DeleteMode::Permanent => std::fs::remove_file(path).map_err(|e| e.to_string()),
        };
        match result {
            Ok(()) => {
                outcome.deleted.push(path.clone());
                outcome.freed += target.size;
                append_audit_log(&log_path, path, target.size);
            }
            Err(msg) => outcome.failed.push((path.clone(), msg)),
        }
    }

    outcome
}

/// 把文件移入 `<root>/.reclaim-trash/<相对路径>`，重名则追加 `.1/.2…`。
fn move_to_trash(path: &Path, root: &Path, trash_root: &Path) -> Result<(), String> {
    let rel = path.strip_prefix(root).map_err(|e| e.to_string())?;
    let dest = trash_root.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let dest = unique_dest(dest);
    std::fs::rename(path, &dest).map_err(|e| e.to_string())
}

/// 若目标已存在，追加数字后缀直到不冲突。
fn unique_dest(dest: PathBuf) -> PathBuf {
    if !dest.exists() {
        return dest;
    }
    for n in 1..10_000u32 {
        let mut s = dest.clone().into_os_string();
        s.push(format!(".{n}"));
        let candidate = PathBuf::from(s);
        if !candidate.exists() {
            return candidate;
        }
    }
    dest
}

/// 追加一行审计日志「时间(unix秒) + 大小 + 路径」；best-effort，写失败不影响删除结果。
fn append_audit_log(log_path: &Path, path: &Path, size: u64) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = writeln!(file, "{secs}\t{size}\t{}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            size,
            kind: EntryKind::File,
        }
    }

    fn group(paths: &[&str]) -> DuplicateGroup {
        DuplicateGroup {
            hash: [0u8; 32],
            size: 10,
            paths: paths.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn plan_includes_only_marked_and_sums_sizes() {
        let entries = vec![file("/r/a", 10), file("/r/b", 20), file("/r/c", 30)];
        let mut marked = HashSet::new();
        marked.insert(PathBuf::from("/r/a"));
        marked.insert(PathBuf::from("/r/c"));

        let plan = DeletePlan::build(&marked, &entries);

        assert_eq!(plan.targets.len(), 2);
        assert_eq!(plan.reclaimable, 40);
    }

    #[test]
    fn keeps_one_when_whole_group_marked() {
        let root = Path::new("/r");
        let entries = vec![file("/r/a", 10), file("/r/b", 10)];
        let dups = vec![group(&["/r/a", "/r/b"])];
        let mut marked = HashSet::new();
        marked.insert(PathBuf::from("/r/a"));
        marked.insert(PathBuf::from("/r/b"));

        let safe = plan_safe_deletion(&marked, &entries, &dups, root);

        assert_eq!(safe.targets.len(), 1, "整组标记必须保留一份");
        assert_eq!(safe.kept_for_safety.len(), 1);
    }

    #[test]
    fn rejects_paths_outside_root() {
        let root = Path::new("/r");
        let entries = vec![file("/r/inside", 10), file("/other/outside", 20)];
        let mut marked = HashSet::new();
        marked.insert(PathBuf::from("/r/inside"));
        marked.insert(PathBuf::from("/other/outside"));

        let safe = plan_safe_deletion(&marked, &entries, &[], root);

        assert_eq!(safe.targets.len(), 1);
        assert!(safe.targets.iter().all(|e| e.path.starts_with("/r")));
    }

    #[test]
    fn trash_moves_file_into_isolation_dir() {
        let dir = tempfile::tempdir().expect("temp");
        let root = dir.path();
        let f = root.join("big.bin");
        fs::write(&f, vec![0u8; 100]).expect("write");

        let targets = vec![FileEntry {
            path: f.clone(),
            size: 100,
            kind: EntryKind::File,
        }];
        let outcome = execute_deletion(&targets, DeleteMode::Trash, root);

        assert_eq!(outcome.deleted.len(), 1);
        assert_eq!(outcome.freed, 100);
        assert!(outcome.failed.is_empty());
        assert!(!f.exists(), "原位文件应已移走");
        assert!(root.join(".reclaim-trash/big.bin").exists(), "应进入回收区");
        assert!(root.join("reclaim-deleted.log").exists(), "应有审计日志");
    }

    #[test]
    fn permanent_removes_file() {
        let dir = tempfile::tempdir().expect("temp");
        let root = dir.path();
        let f = root.join("gone.bin");
        fs::write(&f, b"x").expect("write");

        let targets = vec![FileEntry {
            path: f.clone(),
            size: 1,
            kind: EntryKind::File,
        }];
        let outcome = execute_deletion(&targets, DeleteMode::Permanent, root);

        assert_eq!(outcome.deleted.len(), 1);
        assert!(!f.exists());
        assert!(!root.join(".reclaim-trash").exists(), "永久删除不入回收区");
    }

    #[test]
    fn missing_target_is_recorded_as_failed() {
        let dir = tempfile::tempdir().expect("temp");
        let root = dir.path();
        let targets = vec![FileEntry {
            path: root.join("nope.bin"),
            size: 5,
            kind: EntryKind::File,
        }];
        let outcome = execute_deletion(&targets, DeleteMode::Trash, root);

        assert!(outcome.deleted.is_empty());
        assert_eq!(outcome.failed.len(), 1);
    }
}
