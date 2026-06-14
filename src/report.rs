use crate::model::{DuplicateGroup, EntryKind, FileEntry, ScanResult};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ffi::OsString;

/// 把字节数转成人类可读字符串，如 1536 → "1.5 KB"。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// 按大小降序排序，返回最大的 `top` 个条目。
pub fn top_entries(entries: &[FileEntry], top: usize) -> Vec<&FileEntry> {
    let mut sorted: Vec<&FileEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| Reverse(e.size));
    sorted.truncate(top);
    sorted
}

/// 渲染扫描结果（外加可选的重复文件分组）的输出后端。
pub trait Reporter {
    fn report(&self, result: &ScanResult, dups: Option<&[DuplicateGroup]>, top: usize);
}

/// 表格：按大小降序列出 top-N 项 + 总计。
pub struct TableReporter;

/// 树形：还原目录层级，目录展示子孙文件聚合大小。
pub struct TreeReporter;

/// JSON：机器可读的单个文档。
pub struct JsonReporter;

impl Reporter for TableReporter {
    fn report(&self, result: &ScanResult, dups: Option<&[DuplicateGroup]>, top: usize) {
        for entry in top_entries(&result.entries, top) {
            println!("{:>10}  {}", human_size(entry.size), entry.path.display());
        }
        println!("total: {}", human_size(result.total_size));

        if let Some(groups) = dups {
            render_duplicates(groups);
        }
        eprint_warnings(result);
    }
}

impl Reporter for TreeReporter {
    fn report(&self, result: &ScanResult, dups: Option<&[DuplicateGroup]>, top: usize) {
        let tree = build_tree(result);
        let root_name = result.root.display().to_string();
        print_node(&tree, &root_name, 0, top);

        if let Some(groups) = dups {
            render_duplicates(groups);
        }
        eprint_warnings(result);
    }
}

impl Reporter for JsonReporter {
    fn report(&self, result: &ScanResult, dups: Option<&[DuplicateGroup]>, top: usize) {
        match render_json(result, dups, top) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("JSON 序列化失败: {e}"),
        }
    }
}

/// 把重复文件分组按「可回收空间」降序打印，并给出总可回收量。
fn render_duplicates(groups: &[DuplicateGroup]) {
    if groups.is_empty() {
        println!("未发现重复文件");
        return;
    }

    // 可回收空间 = 单份大小 ×（份数 - 1），降序展示最值得清理的组。
    let mut groups: Vec<&DuplicateGroup> = groups.iter().collect();
    groups.sort_by_key(|g| Reverse(g.size * (g.paths.len() as u64 - 1)));

    let mut reclaimable = 0;
    for group in groups {
        let wasted = group.size * (group.paths.len() as u64 - 1);
        reclaimable += wasted;
        println!(
            "{} × {} 份（可回收 {}）",
            human_size(group.size),
            group.paths.len(),
            human_size(wasted)
        );
        for path in &group.paths {
            println!("  {}", path.display());
        }
    }
    println!("总计可回收: {}", human_size(reclaimable));
}

fn eprint_warnings(result: &ScanResult) {
    for warning in &result.warnings {
        eprintln!("warning: {warning}");
    }
}

// ---- 树形渲染 ----

struct Node {
    size: u64,
    is_dir: bool,
    children: BTreeMap<OsString, Node>,
}

fn build_tree(result: &ScanResult) -> Node {
    let mut root = Node {
        size: 0,
        is_dir: true,
        children: BTreeMap::new(),
    };
    for entry in &result.entries {
        let rel = entry
            .path
            .strip_prefix(&result.root)
            .unwrap_or(entry.path.as_path());
        let comps: Vec<OsString> = rel
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        insert(&mut root, &comps, entry);
    }
    aggregate(&mut root);
    root
}

fn insert(node: &mut Node, comps: &[OsString], entry: &FileEntry) {
    let Some((first, rest)) = comps.split_first() else {
        return;
    };
    let child = node.children.entry(first.clone()).or_insert_with(|| Node {
        size: 0,
        is_dir: true,
        children: BTreeMap::new(),
    });
    if rest.is_empty() {
        match entry.kind {
            EntryKind::File => {
                child.is_dir = false;
                child.size = entry.size;
            }
            EntryKind::Symlink => {
                child.is_dir = false;
                child.size = 0;
            }
            EntryKind::Dir => child.is_dir = true,
        }
    } else {
        insert(child, rest, entry);
    }
}

/// 后序聚合：目录大小 = 全部子孙文件大小之和。
fn aggregate(node: &mut Node) -> u64 {
    if !node.is_dir {
        return node.size;
    }
    let mut sum = 0;
    for child in node.children.values_mut() {
        sum += aggregate(child);
    }
    node.size = sum;
    sum
}

fn print_node(node: &Node, name: &str, depth: usize, top: usize) {
    let indent = "  ".repeat(depth);
    let slash = if node.is_dir { "/" } else { "" };
    println!("{indent}{}  {name}{slash}", human_size(node.size));

    let mut kids: Vec<(&OsString, &Node)> = node.children.iter().collect();
    kids.sort_by_key(|(_, n)| Reverse(n.size));
    for (cname, child) in kids.into_iter().take(top) {
        let label = cname.to_string_lossy();
        print_node(child, &label, depth + 1, top);
    }
}

// ---- JSON 渲染 ----

#[derive(Serialize)]
struct JsonEntry {
    path: String,
    size: u64,
    kind: &'static str,
}

#[derive(Serialize)]
struct JsonDup {
    hash: String,
    size: u64,
    paths: Vec<String>,
}

#[derive(Serialize)]
struct JsonReport {
    root: String,
    total_size: u64,
    entries: Vec<JsonEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duplicates: Option<Vec<JsonDup>>,
    warnings: Vec<String>,
}

fn kind_str(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Symlink => "symlink",
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn render_json(
    result: &ScanResult,
    dups: Option<&[DuplicateGroup]>,
    top: usize,
) -> Result<String, serde_json::Error> {
    let entries = top_entries(&result.entries, top)
        .iter()
        .map(|e| JsonEntry {
            path: e.path.display().to_string(),
            size: e.size,
            kind: kind_str(e.kind),
        })
        .collect();

    let duplicates = dups.map(|groups| {
        groups
            .iter()
            .map(|g| JsonDup {
                hash: hex(&g.hash),
                size: g.size,
                paths: g.paths.iter().map(|p| p.display().to_string()).collect(),
            })
            .collect()
    });

    let report = JsonReport {
        root: result.root.display().to_string(),
        total_size: result.total_size,
        entries,
        duplicates,
        warnings: result.warnings.iter().map(|w| w.to_string()).collect(),
    };

    serde_json::to_string_pretty(&report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryKind;
    use std::path::PathBuf;

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn top_entries_sorts_desc_and_truncates() {
        let entries = vec![
            FileEntry {
                path: PathBuf::from("a"),
                size: 10,
                kind: EntryKind::File,
            },
            FileEntry {
                path: PathBuf::from("b"),
                size: 30,
                kind: EntryKind::File,
            },
            FileEntry {
                path: PathBuf::from("c"),
                size: 20,
                kind: EntryKind::File,
            },
        ];

        let top = top_entries(&entries, 2);

        assert_eq!(top.len(), 2);
        assert_eq!(top[0].size, 30);
        assert_eq!(top[1].size, 20);
    }

    #[test]
    fn render_json_is_valid_and_has_fields() {
        let result = ScanResult {
            root: PathBuf::from("/root"),
            entries: vec![
                FileEntry {
                    path: PathBuf::from("/root/a"),
                    size: 30,
                    kind: EntryKind::File,
                },
                FileEntry {
                    path: PathBuf::from("/root/b"),
                    size: 10,
                    kind: EntryKind::File,
                },
            ],
            total_size: 40,
            warnings: Vec::new(),
        };
        let dups = vec![DuplicateGroup {
            hash: [0xab; 32],
            size: 30,
            paths: vec![PathBuf::from("/root/a"), PathBuf::from("/root/c")],
        }];

        let json = render_json(&result, Some(&dups), 20).expect("serialize json");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");

        assert_eq!(parsed["total_size"].as_u64().unwrap(), 40);
        assert_eq!(parsed["entries"][0]["size"].as_u64().unwrap(), 30); // 最大的排在最前
        assert_eq!(parsed["entries"][0]["kind"].as_str().unwrap(), "file");
        assert_eq!(
            parsed["duplicates"][0]["paths"].as_array().unwrap().len(),
            2
        );
        assert_eq!(parsed["duplicates"][0]["hash"].as_str().unwrap().len(), 64);
    }
}
