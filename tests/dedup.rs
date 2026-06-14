use reclaim::dedup::find_duplicates;
use reclaim::scanner::{ScanOptions, scan};
use std::fs;

#[test]
fn finds_the_known_duplicate_pair() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let root = dir.path();

    // 两个内容完全相同的文件（应被识别为一组重复）。
    let payload = b"reclaim duplicate detection payload";
    fs::write(root.join("copy1.txt"), payload).expect("write copy1");
    fs::write(root.join("nested_copy.txt"), payload).expect("write nested_copy");

    // 同样大小但内容不同（不得误报）。
    let other: Vec<u8> = (0..payload.len()).map(|i| (i % 251) as u8).collect();
    assert_eq!(other.len(), payload.len());
    fs::write(root.join("different.txt"), &other).expect("write different");

    // 大小独一无二（第 1 级即排除）。
    fs::write(root.join("unique.txt"), b"x").expect("write unique");

    let options = ScanOptions {
        threads: 4,
        min_size: 0,
        include_hidden: false,
    };
    let result = scan(root, &options);
    let (groups, warnings) = find_duplicates(&result.entries, 4);

    assert!(warnings.is_empty(), "warnings: {warnings:?}");
    assert_eq!(groups.len(), 1, "应恰好一组重复, 实际: {groups:?}");

    let group = &groups[0];
    assert_eq!(group.size, payload.len() as u64);
    assert_eq!(group.paths.len(), 2);
    assert!(group.paths.iter().any(|p| p.ends_with("copy1.txt")));
    assert!(group.paths.iter().any(|p| p.ends_with("nested_copy.txt")));
}
