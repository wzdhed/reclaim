use crate::error::ScanError;
use crate::model::{EntryKind, FileEntry, ScanResult};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

/// 扫描的过滤与并发选项。
#[derive(Clone, Copy)]
pub struct ScanOptions {
    /// 工作线程数。
    pub threads: usize,
    /// 忽略小于该大小的文件。
    pub min_size: u64,
    /// 是否包含隐藏文件/目录。
    pub include_hidden: bool,
}

/// worker 间共享的工作状态。
struct Shared {
    /// 待处理的目录队列。
    queue: VecDeque<PathBuf>,
    /// 已入队但尚未处理完的目录总数（= 队列中 + 正在处理中）。归零即全部完成。
    pending: usize,
}

/// 流式扫描中产出的单项：一个条目或一条错误。
pub enum ScanItem {
    Entry(FileEntry),
    Warning(ScanError),
}

/// 多线程并发扫描 `root` 下的整棵目录树，扫完聚合成 `ScanResult`。
///
/// 单个文件/目录的错误收集进 `ScanResult.warnings`，不中断整次扫描。
pub fn scan(root: &Path, options: &ScanOptions) -> ScanResult {
    let mut result = ScanResult {
        root: root.to_path_buf(),
        entries: Vec::new(),
        total_size: 0,
        warnings: Vec::new(),
    };
    scan_stream(root, options, |item| match item {
        ScanItem::Entry(entry) => {
            if entry.kind == EntryKind::File {
                result.total_size += entry.size;
            }
            result.entries.push(entry);
        }
        ScanItem::Warning(warning) => result.warnings.push(warning),
    });
    result
}

/// 流式扫描：每发现一个条目/错误就调用 `sink`，不在内部聚合。
///
/// 共享工作队列 + 自建线程池 + mpsc channel：worker 从队列取目录，
/// 文件经 channel 发回收集方，子目录塞回队列让其他线程接手。
/// `sink` 在调用线程上被逐条调用，本函数返回时整棵树已扫完。
pub fn scan_stream(root: &Path, options: &ScanOptions, mut sink: impl FnMut(ScanItem)) {
    let threads = options.threads.max(1);
    let opts = *options;

    let shared = Arc::new(Mutex::new(Shared {
        queue: VecDeque::from([root.to_path_buf()]),
        pending: 1,
    }));
    let cvar = Arc::new(Condvar::new());
    let (tx, rx) = channel::<ScanItem>();

    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let shared = Arc::clone(&shared);
        let cvar = Arc::clone(&cvar);
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            worker(opts, &shared, &cvar, &tx)
        }));
    }
    // 收集方必须 drop 掉自己这份发送端，否则 channel 永不关闭。
    drop(tx);

    while let Ok(item) = rx.recv() {
        sink(item);
    }

    for handle in handles {
        let _ = handle.join();
    }
}

/// 文件名以 `.` 开头即视为隐藏（跨平台一致）。
fn is_hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn worker(options: ScanOptions, shared: &Mutex<Shared>, cvar: &Condvar, tx: &Sender<ScanItem>) {
    loop {
        // 从共享队列取一个目录；队列空但仍有目录在处理则等待，全部完成则退出。
        let dir = {
            let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(dir) = state.queue.pop_front() {
                    break Some(dir);
                }
                if state.pending == 0 {
                    break None;
                }
                state = cvar.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
        };

        let dir = match dir {
            Some(dir) => dir,
            None => {
                // 没有更多任务，唤醒其他等待者一起退出。
                cvar.notify_all();
                break;
            }
        };

        process_dir(&dir, options, shared, cvar, tx);

        // 当前目录处理完毕，更新计数；归零说明整棵树扫完。
        let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
        state.pending -= 1;
        if state.pending == 0 {
            cvar.notify_all();
        }
    }
}

fn process_dir(
    dir: &Path,
    options: ScanOptions,
    shared: &Mutex<Shared>,
    cvar: &Condvar,
    tx: &Sender<ScanItem>,
) {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(source) => {
            let _ = tx.send(ScanItem::Warning(ScanError::Io {
                path: dir.to_path_buf(),
                source,
            }));
            return;
        }
    };

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(source) => {
                let _ = tx.send(ScanItem::Warning(ScanError::Io {
                    path: dir.to_path_buf(),
                    source,
                }));
                continue;
            }
        };

        // 隐藏过滤：跳过隐藏文件，跳过隐藏目录即不递归。
        if !options.include_hidden && is_hidden(&entry.file_name()) {
            continue;
        }

        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(source) => {
                let _ = tx.send(ScanItem::Warning(ScanError::Io { path, source }));
                continue;
            }
        };

        if file_type.is_dir() {
            let _ = tx.send(ScanItem::Entry(FileEntry {
                path: path.clone(),
                size: 0,
                kind: EntryKind::Dir,
            }));
            // 把子目录塞回队列让其他线程接手。
            let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
            state.queue.push_back(path);
            state.pending += 1;
            cvar.notify_one();
        } else if file_type.is_symlink() {
            let _ = tx.send(ScanItem::Entry(FileEntry {
                path,
                size: 0,
                kind: EntryKind::Symlink,
            }));
        } else {
            let size = match entry.metadata() {
                Ok(meta) => meta.len(),
                Err(source) => {
                    let _ = tx.send(ScanItem::Warning(ScanError::Io { path, source }));
                    continue;
                }
            };
            // min-size 过滤：小于阈值的文件当它不存在。
            if size < options.min_size {
                continue;
            }
            let _ = tx.send(ScanItem::Entry(FileEntry {
                path,
                size,
                kind: EntryKind::File,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(threads: usize) -> ScanOptions {
        ScanOptions {
            threads,
            min_size: 0,
            include_hidden: false,
        }
    }

    #[test]
    fn scanning_nonexistent_path_collects_warning() {
        let result = scan(Path::new("this/path/does/not/exist"), &opts(4));
        assert!(result.entries.is_empty());
        assert_eq!(result.total_size, 0);
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn skips_hidden_unless_requested() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        std::fs::write(root.join("visible.txt"), b"hello").expect("write visible");
        std::fs::write(root.join(".secret"), b"hi").expect("write hidden");

        let default = scan(root, &opts(2));
        assert!(
            default
                .entries
                .iter()
                .any(|e| e.path.ends_with("visible.txt"))
        );
        assert!(!default.entries.iter().any(|e| e.path.ends_with(".secret")));

        let with_hidden = scan(
            root,
            &ScanOptions {
                include_hidden: true,
                ..opts(2)
            },
        );
        assert!(
            with_hidden
                .entries
                .iter()
                .any(|e| e.path.ends_with(".secret"))
        );
    }

    #[test]
    fn scan_stream_matches_scan() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        std::fs::write(root.join("a.txt"), b"hello").expect("write a");
        let sub = root.join("sub");
        std::fs::create_dir(&sub).expect("create sub");
        std::fs::write(sub.join("b.txt"), b"world!!").expect("write b");

        let aggregated = scan(root, &opts(4));

        let mut streamed_entries = 0;
        let mut streamed_total = 0u64;
        scan_stream(root, &opts(4), |item| {
            if let ScanItem::Entry(e) = item {
                streamed_entries += 1;
                if e.kind == EntryKind::File {
                    streamed_total += e.size;
                }
            }
        });

        assert_eq!(streamed_entries, aggregated.entries.len());
        assert_eq!(streamed_total, aggregated.total_size);
        assert_eq!(aggregated.total_size, 5 + 7);
    }

    #[test]
    fn min_size_filters_small_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        std::fs::write(root.join("big.bin"), vec![0u8; 2048]).expect("write big");
        std::fs::write(root.join("small.bin"), vec![0u8; 100]).expect("write small");

        let result = scan(
            root,
            &ScanOptions {
                min_size: 1024,
                ..opts(2)
            },
        );
        assert!(result.entries.iter().any(|e| e.path.ends_with("big.bin")));
        assert!(!result.entries.iter().any(|e| e.path.ends_with("small.bin")));
        assert_eq!(result.total_size, 2048);
    }
}
