use reclaim::model::EntryKind;
use reclaim::scanner::{ScanOptions, scan};
use std::fs;
use std::path::Path;

fn opts(threads: usize) -> ScanOptions {
    ScanOptions {
        threads,
        min_size: 0,
        include_hidden: false,
    }
}

fn count_files(root: &Path, threads: usize) -> usize {
    scan(root, &opts(threads))
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .count()
}

#[test]
fn scans_known_tree_and_sums_sizes() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let root = dir.path();

    // 顶层两个文件
    fs::write(root.join("a.txt"), b"hello").expect("write a.txt"); // 5 字节
    fs::write(root.join("b.txt"), b"world!!").expect("write b.txt"); // 7 字节

    // 子目录 + 其中一个文件
    let sub = root.join("sub");
    fs::create_dir(&sub).expect("create sub dir");
    fs::write(sub.join("c.txt"), b"abc").expect("write c.txt"); // 3 字节

    let result = scan(root, &opts(4));

    assert!(
        result.warnings.is_empty(),
        "warnings: {:?}",
        result.warnings
    );
    assert_eq!(result.total_size, 5 + 7 + 3);

    let file_count = result
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .count();
    assert_eq!(file_count, 3);

    let has_sub_dir = result
        .entries
        .iter()
        .any(|e| e.kind == EntryKind::Dir && e.path.ends_with("sub"));
    assert!(has_sub_dir, "未找到子目录 sub");
}

#[test]
fn concurrent_scan_is_consistent() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let root = dir.path();

    // 造一棵多层、多文件的树：4 个子目录、每个 10 个文件，再加一层更深的嵌套。
    let mut expected_total = 0u64;
    let mut expected_files = 0usize;
    for d in 0..4 {
        let sub = root.join(format!("dir{d}"));
        fs::create_dir(&sub).expect("create sub dir");
        for f in 0..10 {
            let content = format!("file-{d}-{f}-payload");
            fs::write(sub.join(format!("f{f}.txt")), &content).expect("write file");
            expected_total += content.len() as u64;
            expected_files += 1;

            // 再嵌一层目录，验证跨层级的任务派发。
            let deep = sub.join(format!("deep{f}"));
            fs::create_dir(&deep).expect("create deep dir");
            let deep_content = format!("deep-{d}-{f}");
            fs::write(deep.join("g.txt"), &deep_content).expect("write deep file");
            expected_total += deep_content.len() as u64;
            expected_files += 1;
        }
    }

    let single = scan(root, &opts(1));
    let multi = scan(root, &opts(8));

    // 单线程结果先和已知量对齐。
    assert!(
        single.warnings.is_empty(),
        "warnings: {:?}",
        single.warnings
    );
    assert_eq!(single.total_size, expected_total);
    assert_eq!(count_files(root, 1), expected_files);

    // 多线程结果必须与单线程完全一致（无丢失/重复/死锁）。
    assert!(multi.warnings.is_empty(), "warnings: {:?}", multi.warnings);
    assert_eq!(multi.total_size, single.total_size);
    assert_eq!(count_files(root, 8), expected_files);
}
