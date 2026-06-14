use crate::error::ScanError;
use crate::model::{DuplicateGroup, EntryKind, FileEntry};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, PoisonError};

/// 头部哈希读取的字节数。
const HEAD_BYTES: u64 = 4096;

/// 三级过滤找出内容完全相同的重复文件。
///
/// 1. 按大小分组，大小独一无二的直接排除；
/// 2. 对同大小的候选并行算前 4 KB 头部哈希，头部不同的排除；
/// 3. 对头部也相同的候选并行算全量哈希，全量相同即内容重复。
///
/// 哈希过程中读不了的文件收集进返回的 warnings，不中断检测。
pub fn find_duplicates(
    entries: &[FileEntry],
    threads: usize,
) -> (Vec<DuplicateGroup>, Vec<ScanError>) {
    let mut warnings = Vec::new();

    // path -> size，供后续按 size 分组与构造 DuplicateGroup 取用。
    let mut size_of: HashMap<PathBuf, u64> = HashMap::new();

    // 第 1 级：按大小分组，仅保留 size 相同且 >= 2 个的候选。
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for entry in entries {
        if entry.kind == EntryKind::File {
            size_of.insert(entry.path.clone(), entry.size);
            by_size
                .entry(entry.size)
                .or_default()
                .push(entry.path.clone());
        }
    }
    let candidates: Vec<PathBuf> = by_size
        .into_values()
        .filter(|paths| paths.len() >= 2)
        .flatten()
        .collect();

    // 第 2 级：头部哈希，按 (size, head) 再分组，仅保留 >= 2 个的候选。
    let (head_hashes, head_warnings) = parallel_hash(candidates, HEAD_BYTES, threads);
    warnings.extend(head_warnings);

    let mut by_head: HashMap<(u64, [u8; 32]), Vec<PathBuf>> = HashMap::new();
    for (path, head) in head_hashes {
        let size = size_of.get(&path).copied().unwrap_or(0);
        by_head.entry((size, head)).or_default().push(path);
    }
    let candidates: Vec<PathBuf> = by_head
        .into_values()
        .filter(|paths| paths.len() >= 2)
        .flatten()
        .collect();

    // 第 3 级：全量哈希，按全量哈希分组，组内 >= 2 个即内容重复。
    let (full_hashes, full_warnings) = parallel_hash(candidates, u64::MAX, threads);
    warnings.extend(full_warnings);

    let mut by_full: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
    for (path, full) in full_hashes {
        by_full.entry(full).or_default().push(path);
    }

    let mut groups = Vec::new();
    for (hash, paths) in by_full {
        if paths.len() >= 2 {
            let size = size_of.get(&paths[0]).copied().unwrap_or(0);
            groups.push(DuplicateGroup { hash, size, paths });
        }
    }

    (groups, warnings)
}

/// 对单个文件算 blake3 哈希，最多读取 `limit` 字节（全量传 `u64::MAX`）。
fn hash_file(path: &Path, limit: u64) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 8192];
    let mut remaining = limit;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(*hasher.finalize().as_bytes())
}

/// 把一批固定的文件丢进线程池并行算哈希（任务集不增长，队列空即退出）。
fn parallel_hash(
    paths: Vec<PathBuf>,
    limit: u64,
    threads: usize,
) -> (Vec<(PathBuf, [u8; 32])>, Vec<ScanError>) {
    if paths.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let threads = threads.max(1);

    let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
    let (tx, rx) = channel::<Result<(PathBuf, [u8; 32]), ScanError>>();

    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let path = {
                    let mut q = queue.lock().unwrap_or_else(PoisonError::into_inner);
                    q.pop_front()
                };
                let path = match path {
                    Some(p) => p,
                    None => break,
                };
                let msg = match hash_file(&path, limit) {
                    Ok(hash) => Ok((path, hash)),
                    Err(source) => Err(ScanError::Io { path, source }),
                };
                let _ = tx.send(msg);
            }
        }));
    }
    drop(tx);

    let mut hashed = Vec::new();
    let mut warnings = Vec::new();
    while let Ok(msg) = rx.recv() {
        match msg {
            Ok(pair) => hashed.push(pair),
            Err(e) => warnings.push(e),
        }
    }
    for handle in handles {
        let _ = handle.join();
    }

    (hashed, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{ScanOptions, scan};
    use std::fs;

    fn opts(threads: usize) -> ScanOptions {
        ScanOptions {
            threads,
            min_size: 0,
            include_hidden: false,
        }
    }

    #[test]
    fn finds_identical_and_ignores_others() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();

        // 一对内容完全相同的文件。
        fs::write(root.join("x1.bin"), vec![b'A'; 1000]).expect("write x1");
        fs::write(root.join("x2.bin"), vec![b'A'; 1000]).expect("write x2");
        // 同样大小但内容不同（应在头部哈希阶段被排除）。
        fs::write(root.join("y.bin"), vec![b'B'; 1000]).expect("write y");
        // 大小独一无二（应在第 1 级被排除）。
        fs::write(root.join("z.bin"), vec![b'C'; 7]).expect("write z");

        let result = scan(root, &opts(4));
        let (groups, warnings) = find_duplicates(&result.entries, 4);

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths.len(), 2);
        assert_eq!(groups[0].size, 1000);
    }

    #[test]
    fn same_head_different_tail_not_duplicate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();

        // 前 5000 字节相同（头部 4 KB 一致），仅最后一字节不同。
        let mut a = vec![b'X'; 5000];
        let mut b = a.clone();
        *a.last_mut().expect("non-empty") = b'1';
        *b.last_mut().expect("non-empty") = b'2';
        fs::write(root.join("a.bin"), &a).expect("write a");
        fs::write(root.join("b.bin"), &b).expect("write b");

        let result = scan(root, &opts(4));
        let (groups, warnings) = find_duplicates(&result.entries, 4);

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert!(groups.is_empty(), "全量哈希不同不应判为重复: {groups:?}");
    }

    #[test]
    fn hash_file_is_consistent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        fs::write(root.join("p.bin"), b"same payload").expect("write p");
        fs::write(root.join("q.bin"), b"same payload").expect("write q");
        fs::write(root.join("r.bin"), b"diff payload").expect("write r");

        let p = hash_file(&root.join("p.bin"), u64::MAX).expect("hash p");
        let q = hash_file(&root.join("q.bin"), u64::MAX).expect("hash q");
        let r = hash_file(&root.join("r.bin"), u64::MAX).expect("hash r");

        assert_eq!(p, q);
        assert_ne!(p, r);
    }
}
