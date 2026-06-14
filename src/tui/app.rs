use crate::actions::{self, DeleteMode, DeleteOutcome, DeletePlan, SafeDeletion};
use crate::error::ScanError;
use crate::model::{DuplicateGroup, EntryKind, FileEntry, ScanResult};
use crossterm::event::{KeyCode, KeyEvent};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 当前内容视图。Help 不在此枚举里——它是覆盖在内容之上的浮层（见 `App::show_help`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Sizes,
    Duplicates,
}

/// Sizes 视图的排序方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Size,
    Name,
}

/// Sizes 视图里「当前目录」下的一个子项。
pub struct Row {
    pub path: PathBuf,
    /// 文件为自身大小，目录为子孙文件聚合大小。
    pub size: u64,
    pub is_dir: bool,
}

/// TUI 的状态机（纯逻辑，可单测）。扫描边进行边通过 `add_entries` 等方法增量更新。
pub struct App {
    pub result: ScanResult,
    /// 每个目录的聚合大小（子孙文件之和），随扫描增量累加。
    dir_sizes: HashMap<PathBuf, u64>,
    /// 当前正在浏览的目录（不会越过 `result.root`）。
    pub cwd: PathBuf,
    /// 当前目录的直接子项，已按 `sort` 排好。
    pub rows: Vec<Row>,
    /// 重复文件分组，按可回收空间降序排好。
    pub dups: Vec<DuplicateGroup>,
    pub view: View,
    pub sort: SortKey,
    /// 是否显示帮助浮层。
    pub show_help: bool,
    /// 当前视图里的高亮行。
    pub selected: usize,
    pub should_quit: bool,
    /// 后台扫描是否仍在进行。
    pub scanning: bool,
    /// 已扫描到的条目数（进度）。
    pub scanned_count: usize,
    /// 去重是否已算完（扫描结束后才在后台计算）。
    pub dups_ready: bool,
    /// Duplicates 视图：是否聚焦在「选中组的文件」面板上滚动浏览。
    pub dup_focus: bool,
    /// 「选中组的文件」面板里的路径光标。
    pub detail_cursor: usize,
    /// 是否正在显示退出确认浮层。
    pub confirm_quit: bool,
    /// 已标记待删的文件路径。
    pub marked: HashSet<PathBuf>,
    /// 是否正在显示删除预览（dry-run）浮层。
    pub show_plan: bool,
    /// 删除方式（trash / permanent），由 CLI 注入。
    pub mode: DeleteMode,
    /// 是否正在显示删除确认浮层。
    pub confirm_delete: bool,
    /// permanent 模式下是否已按过一次 y（等待第二次确认）。
    pub permanent_armed: bool,
    /// 置位后由 run_tui 执行真正删除（写盘），随后清零。
    pub request_delete: bool,
    /// 删除结果摘要浮层（None = 不显示）。
    pub delete_result: Option<String>,
}

/// 一个重复组的可回收空间 = 单份大小 ×（份数 - 1）。
fn reclaimable(group: &DuplicateGroup) -> u64 {
    group.size * (group.paths.len() as u64 - 1)
}

/// 把一个文件的大小累加到它的每一级祖先目录（到 root inclusive）。
fn accumulate_dir_sizes(map: &mut HashMap<PathBuf, u64>, root: &Path, file: &Path, size: u64) {
    let mut p = file;
    while let Some(parent) = p.parent() {
        *map.entry(parent.to_path_buf()).or_insert(0) += size;
        if parent == root {
            break;
        }
        p = parent;
    }
}

impl App {
    /// 构造一个空的、正在扫描中的 App，定位在扫描根。条目随后通过 `add_entries` 流入。
    pub fn new(root: PathBuf) -> Self {
        let result = ScanResult {
            root: root.clone(),
            entries: Vec::new(),
            total_size: 0,
            warnings: Vec::new(),
        };
        Self {
            result,
            dir_sizes: HashMap::new(),
            cwd: root,
            rows: Vec::new(),
            dups: Vec::new(),
            view: View::Sizes,
            sort: SortKey::Size,
            show_help: false,
            selected: 0,
            should_quit: false,
            scanning: true,
            scanned_count: 0,
            dups_ready: false,
            dup_focus: false,
            detail_cursor: 0,
            confirm_quit: false,
            marked: HashSet::new(),
            show_plan: false,
            mode: DeleteMode::Trash,
            confirm_delete: false,
            permanent_armed: false,
            request_delete: false,
            delete_result: None,
        }
    }

    /// 收到一批扫描条目：累加大小、更新聚合、重建当前目录列表。
    pub fn add_entries(&mut self, batch: Vec<FileEntry>) {
        let root = self.result.root.clone();
        for entry in &batch {
            if entry.kind == EntryKind::File {
                self.result.total_size += entry.size;
                accumulate_dir_sizes(&mut self.dir_sizes, &root, &entry.path, entry.size);
            }
        }
        self.result.entries.extend(batch);
        self.rebuild_rows();
    }

    /// 更新进度计数。
    pub fn set_progress(&mut self, scanned: usize) {
        self.scanned_count = scanned;
    }

    /// 扫描结束：记录 warnings、清扫描标志、最终重建列表。
    pub fn finish_scan(&mut self, warnings: Vec<ScanError>) {
        self.result.warnings = warnings;
        self.scanning = false;
        self.rebuild_rows();
    }

    /// 去重算完：按可回收空间降序存好。
    pub fn set_dups(&mut self, mut dups: Vec<DuplicateGroup>) {
        dups.sort_by_key(|g| Reverse(reclaimable(g)));
        self.dups = dups;
        self.dups_ready = true;
    }

    /// 当前目录的聚合大小（供标题显示）。
    pub fn cwd_size(&self) -> u64 {
        self.dir_sizes.get(&self.cwd).copied().unwrap_or(0)
    }

    /// 当前视图的列表长度。
    fn current_len(&self) -> usize {
        match self.view {
            View::Sizes => self.rows.len(),
            View::Duplicates => self.dups.len(),
        }
    }

    /// 重建当前目录的子项列表（过滤直接子项 + 按当前排序排好）。
    fn rebuild_rows(&mut self) {
        let cwd = self.cwd.as_path();
        let mut rows: Vec<Row> = self
            .result
            .entries
            .iter()
            .filter(|e| e.path.parent() == Some(cwd))
            .map(|e| {
                let is_dir = e.kind == EntryKind::Dir;
                let size = if is_dir {
                    self.dir_sizes.get(&e.path).copied().unwrap_or(0)
                } else {
                    e.size
                };
                Row {
                    path: e.path.clone(),
                    size,
                    is_dir,
                }
            })
            .collect();
        match self.sort {
            SortKey::Size => rows.sort_by_key(|r| Reverse(r.size)),
            SortKey::Name => rows.sort_by(|a, b| a.path.cmp(&b.path)),
        }
        // 扫描中条目还在增长，钳制选中行以防越界。
        if self.selected >= rows.len() {
            self.selected = rows.len().saturating_sub(1);
        }
        self.rows = rows;
    }

    /// 上移一行，顶部钳制在 0。
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// 下移一行，底部钳制在 len-1；空列表不动。
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.current_len() {
            self.selected += 1;
        }
    }

    /// 钻入选中的子目录（仅对目录有效）。
    fn enter(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        self.cwd = row.path.clone();
        self.selected = 0;
        self.rebuild_rows();
    }

    /// 返回上级目录；已在扫描根则不动（不越过根）。
    fn go_up(&mut self) {
        if self.cwd == self.result.root {
            return;
        }
        if let Some(parent) = self.cwd.parent() {
            self.cwd = parent.to_path_buf();
            self.selected = 0;
            self.rebuild_rows();
        }
    }

    /// 在 Sizes / Duplicates 视图间切换，选中行与聚焦状态复位。
    fn toggle_view(&mut self) {
        self.view = match self.view {
            View::Sizes => View::Duplicates,
            View::Duplicates => View::Sizes,
        };
        self.selected = 0;
        self.dup_focus = false;
        self.detail_cursor = 0;
    }

    /// 选中重复组的副本数（用于钳制详情滚动）。
    fn selected_dup_paths(&self) -> usize {
        self.dups.get(self.selected).map_or(0, |g| g.paths.len())
    }

    /// ↑：Duplicates 聚焦时滚动详情，否则在列表里上移。
    fn on_up(&mut self) {
        if self.view == View::Duplicates && self.dup_focus {
            self.detail_cursor = self.detail_cursor.saturating_sub(1);
        } else {
            self.move_up();
            self.detail_cursor = 0;
        }
    }

    /// ↓：Duplicates 聚焦时滚动详情（钳制到最后一行），否则在列表里下移。
    fn on_down(&mut self) {
        if self.view == View::Duplicates && self.dup_focus {
            let max = self.selected_dup_paths().saturating_sub(1);
            if self.detail_cursor < max {
                self.detail_cursor += 1;
            }
        } else {
            self.move_down();
            self.detail_cursor = 0;
        }
    }

    /// Enter：Sizes 进目录；Duplicates 聚焦到选中组的文件面板。
    fn on_enter(&mut self) {
        match self.view {
            View::Sizes => self.enter(),
            View::Duplicates => {
                if self.selected_dup_paths() > 0 {
                    self.dup_focus = true;
                    self.detail_cursor = 0;
                }
            }
        }
    }

    /// Backspace：Sizes 返回上级；Duplicates 退出文件浏览聚焦。
    fn on_back(&mut self) {
        match self.view {
            View::Sizes => self.go_up(),
            View::Duplicates => self.dup_focus = false,
        }
    }

    /// 切换排序方式并重排，选中行复位。
    fn toggle_sort(&mut self) {
        self.sort = match self.sort {
            SortKey::Size => SortKey::Name,
            SortKey::Name => SortKey::Size,
        };
        self.selected = 0;
        self.rebuild_rows();
    }

    /// 处理一次按键。
    pub fn handle_key(&mut self, key: KeyEvent) {
        // 删除结果摘要浮层：任意键关闭（最高优先级）。
        if self.delete_result.is_some() {
            self.delete_result = None;
            return;
        }

        // 删除确认浮层：默认不删，需主动确认（permanent 双 y）。
        if self.confirm_delete {
            self.handle_confirm_delete(key);
            return;
        }

        // 退出确认浮层：需主动确认才退出，防误触。
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.should_quit = true,
                KeyCode::Char('n') | KeyCode::Esc => self.confirm_quit = false,
                _ => {}
            }
            return;
        }

        // 删除预览浮层（dry-run）：Enter 进入确认流程，Esc/d 关闭，绝不在此删除。
        if self.show_plan {
            match key.code {
                KeyCode::Enter => {
                    self.show_plan = false;
                    self.confirm_delete = true;
                    self.permanent_armed = false;
                }
                KeyCode::Esc | KeyCode::Char('d') => self.show_plan = false,
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.confirm_quit = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Esc => {
                if self.show_help {
                    self.show_help = false;
                } else if self.dup_focus {
                    self.dup_focus = false;
                } else {
                    self.confirm_quit = true;
                }
            }
            // 帮助浮层打开时，吞掉其余按键（先按 ?/Esc 关闭）。
            _ if self.show_help => {}
            KeyCode::Tab => self.toggle_view(),
            KeyCode::Char('s') => self.toggle_sort(),
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('d') => self.show_plan = true,
            KeyCode::Up => self.on_up(),
            KeyCode::Down => self.on_down(),
            KeyCode::Enter => self.on_enter(),
            KeyCode::Backspace => self.on_back(),
            _ => {}
        }
    }

    /// Space：标记/取消标记当前文件（Sizes 选中文件，或 Duplicates 聚焦时的当前副本）。
    fn toggle_mark(&mut self) {
        let target = match self.view {
            View::Sizes => self
                .rows
                .get(self.selected)
                .filter(|r| !r.is_dir)
                .map(|r| r.path.clone()),
            View::Duplicates if self.dup_focus => self
                .dups
                .get(self.selected)
                .and_then(|g| g.paths.get(self.detail_cursor))
                .cloned(),
            View::Duplicates => None,
        };
        if let Some(path) = target
            && !self.marked.remove(&path)
        {
            self.marked.insert(path);
        }
    }

    /// 根据已标记集合构造删除预览（只读，供 ui 渲染与单测）。
    pub fn delete_plan(&self) -> DeletePlan {
        DeletePlan::build(&self.marked, &self.result.entries)
    }

    /// 删除确认浮层的按键处理：默认不删，permanent 需连按两次 y。
    fn handle_confirm_delete(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('n') | KeyCode::Esc => {
                self.confirm_delete = false;
                self.permanent_armed = false;
            }
            KeyCode::Char('y') | KeyCode::Enter => match self.mode {
                DeleteMode::Trash => {
                    self.confirm_delete = false;
                    self.request_delete = true;
                }
                DeleteMode::Permanent => {
                    if self.permanent_armed {
                        self.confirm_delete = false;
                        self.permanent_armed = false;
                        self.request_delete = true;
                    } else {
                        self.permanent_armed = true;
                    }
                }
            },
            // permanent 已 armed 时按其它键视为取消（防误触）。
            _ if self.permanent_armed => {
                self.permanent_armed = false;
                self.confirm_delete = false;
            }
            _ => {}
        }
    }

    /// 套用安全保护算出真正可删的集合（供 run_tui 取 targets、供单测）。
    pub fn safe_deletion(&self) -> SafeDeletion {
        actions::plan_safe_deletion(
            &self.marked,
            &self.result.entries,
            &self.dups,
            &self.result.root,
        )
    }

    /// 删除执行完毕后更新内存状态：移除已删条目、刷新视图、生成结果摘要。
    pub fn apply_delete_outcome(&mut self, outcome: DeleteOutcome) {
        let deleted: HashSet<PathBuf> = outcome.deleted.iter().cloned().collect();

        for path in &deleted {
            self.marked.remove(path);
        }
        self.result.entries.retain(|e| !deleted.contains(&e.path));
        for group in &mut self.dups {
            group.paths.retain(|p| !deleted.contains(p));
        }
        self.dups.retain(|g| g.paths.len() >= 2);

        self.recompute_totals();
        self.rebuild_rows();

        let detail_len = self.dups.get(self.selected).map_or(0, |g| g.paths.len());
        if self.detail_cursor >= detail_len {
            self.detail_cursor = detail_len.saturating_sub(1);
        }

        let mut summary = format!(
            "已删除 {} 个文件，释放 {}",
            outcome.deleted.len(),
            crate::report::human_size(outcome.freed)
        );
        if !outcome.failed.is_empty() {
            summary.push_str(&format!("；{} 个失败（见界面）", outcome.failed.len()));
        }
        self.delete_result = Some(summary);
        self.show_plan = false;
        self.confirm_delete = false;
        self.permanent_armed = false;
    }

    /// 删除后从现有条目重算聚合大小与总量。
    fn recompute_totals(&mut self) {
        let root = self.result.root.clone();
        self.dir_sizes.clear();
        self.result.total_size = 0;
        let entries = std::mem::take(&mut self.result.entries);
        for entry in &entries {
            if entry.kind == EntryKind::File {
                self.result.total_size += entry.size;
                accumulate_dir_sizes(&mut self.dir_sizes, &root, &entry.path, entry.size);
            }
        }
        self.result.entries = entries;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn file(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            size,
            kind: EntryKind::File,
        }
    }

    fn dir(path: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            size: 0,
            kind: EntryKind::Dir,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// 扫完一个 App：把 entries 一次性喂进去再 finish。
    fn finished(root: &str, entries: Vec<FileEntry>) -> App {
        let mut app = App::new(PathBuf::from(root));
        app.add_entries(entries);
        app.finish_scan(Vec::new());
        app
    }

    /// 顶层若干文件，全部直接挂在 /root 下。
    fn app_with(sizes: &[u64]) -> App {
        let entries = sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| file(&format!("/root/f{i}"), size))
            .collect();
        finished("/root", entries)
    }

    /// /root 下有 a.txt(10) 与子目录 sub，sub 内有 b.txt(20)、c.txt(30)。
    fn nested_app() -> App {
        finished(
            "/root",
            vec![
                file("/root/a.txt", 10),
                dir("/root/sub"),
                file("/root/sub/b.txt", 20),
                file("/root/sub/c.txt", 30),
            ],
        )
    }

    #[test]
    fn starts_in_scanning_state() {
        let app = App::new(PathBuf::from("/root"));
        assert!(app.scanning);
        assert!(app.result.entries.is_empty());
        assert!(app.rows.is_empty());
        assert!(!app.dups_ready);
    }

    #[test]
    fn streaming_accumulates_then_finishes() {
        let mut app = App::new(PathBuf::from("/root"));
        app.add_entries(vec![file("/root/a", 10), file("/root/b", 30)]);
        app.add_entries(vec![file("/root/c", 20)]);
        assert!(app.scanning);
        assert_eq!(app.result.entries.len(), 3);
        assert_eq!(app.result.total_size, 60);
        assert_eq!(app.rows.len(), 3);
        assert_eq!(app.rows[0].size, 30); // 当前已按大小降序

        app.finish_scan(Vec::new());
        assert!(!app.scanning);
    }

    #[test]
    fn set_dups_marks_ready_and_sorts() {
        let mut app = app_with(&[1]);
        assert!(!app.dups_ready);
        let dups = vec![
            DuplicateGroup {
                hash: [0u8; 32],
                size: 10,
                paths: vec![PathBuf::from("a"), PathBuf::from("b")],
            },
            DuplicateGroup {
                hash: [1u8; 32],
                size: 100,
                paths: vec![PathBuf::from("c"), PathBuf::from("d")],
            },
        ];
        app.set_dups(dups);
        assert!(app.dups_ready);
        assert_eq!(app.dups[0].size, 100); // 可回收更大的排前
    }

    #[test]
    fn rows_sorted_desc_and_start_at_top() {
        let app = app_with(&[10, 30, 20]);
        assert_eq!(app.selected, 0);
        assert_eq!(app.rows[0].size, 30);
        assert_eq!(app.rows[2].size, 10);
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let mut app = app_with(&[1, 2, 3]);
        app.move_up();
        assert_eq!(app.selected, 0);
        app.move_down();
        app.move_down();
        app.move_down();
        assert_eq!(app.selected, 2);
        app.move_up();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn navigation_on_empty_is_safe() {
        let mut app = app_with(&[]);
        app.move_down();
        app.move_up();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn tab_switches_view_and_resets_selected() {
        let mut app = app_with(&[1, 2, 3]);
        app.move_down();
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Duplicates);
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Sizes);
    }

    #[test]
    fn sort_toggle_orders_by_name() {
        let mut app = app_with(&[10, 30, 20]);
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort, SortKey::Name);
        assert_eq!(app.rows[0].path, PathBuf::from("/root/f0"));
        assert_eq!(app.rows[1].path, PathBuf::from("/root/f1"));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn help_overlay_toggles_and_blocks_nav() {
        let mut app = app_with(&[1, 2, 3]);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.show_help);
        app.handle_key(key(KeyCode::Down)); // 帮助打开时导航被吞
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::Char('?')));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn enter_descends_into_selected_dir() {
        let mut app = nested_app();
        assert_eq!(app.rows.len(), 2);
        assert!(app.rows[0].is_dir);
        assert_eq!(app.rows[0].size, 50);

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.cwd, PathBuf::from("/root/sub"));
        assert_eq!(app.selected, 0);
        assert_eq!(app.rows.len(), 2);
        assert_eq!(app.rows[0].size, 30);
    }

    #[test]
    fn backspace_returns_to_parent_and_clamps_at_root() {
        let mut app = nested_app();
        app.handle_key(key(KeyCode::Enter));
        app.move_down();
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.cwd, PathBuf::from("/root"));
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.cwd, PathBuf::from("/root"));
    }

    #[test]
    fn enter_on_file_is_noop() {
        let mut app = nested_app();
        app.move_down();
        assert!(!app.rows[app.selected].is_dir);
        let before = app.cwd.clone();
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.cwd, before);
    }

    #[test]
    fn dir_aggregate_size_is_shown() {
        let app = nested_app();
        let sub = app.rows.iter().find(|r| r.is_dir).expect("找得到 sub 目录");
        assert_eq!(sub.size, 50);
    }

    #[test]
    fn q_asks_confirmation_then_quits() {
        let mut app = app_with(&[1]);
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.confirm_quit);
        assert!(!app.should_quit); // 不直接退出

        app.handle_key(key(KeyCode::Char('n'))); // 取消
        assert!(!app.confirm_quit);
        assert!(!app.should_quit);

        app.handle_key(key(KeyCode::Char('q')));
        app.handle_key(key(KeyCode::Char('y'))); // 确认
        assert!(app.should_quit);
    }

    #[test]
    fn esc_asks_confirmation_and_can_cancel() {
        let mut app = app_with(&[1]);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.confirm_quit);
        assert!(!app.should_quit);

        app.handle_key(key(KeyCode::Esc)); // 确认框里 Esc = 取消
        assert!(!app.confirm_quit);
        assert!(!app.should_quit);
    }

    #[test]
    fn confirm_overlay_swallows_other_keys() {
        let mut app = app_with(&[1, 2, 3]);
        app.move_down(); // selected = 1
        app.handle_key(key(KeyCode::Char('q'))); // 弹确认框
        app.handle_key(key(KeyCode::Down)); // 被吞，不移动
        assert_eq!(app.selected, 1);
        assert!(app.confirm_quit);
    }

    #[test]
    fn space_toggles_mark_on_file_in_sizes() {
        let mut app = app_with(&[10, 20, 30]); // 都是文件
        let path = app.rows[app.selected].path.clone();
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.marked.contains(&path));
        app.handle_key(key(KeyCode::Char(' '))); // 再按取消
        assert!(!app.marked.contains(&path));
    }

    #[test]
    fn space_ignores_directories() {
        let mut app = nested_app(); // rows[0] = sub 目录
        assert!(app.rows[0].is_dir);
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.marked.is_empty());
    }

    #[test]
    fn space_marks_current_copy_in_focused_duplicates() {
        let mut app = app_with(&[1]);
        app.set_dups(vec![group(10, &["a", "b", "c"])]);
        app.handle_key(key(KeyCode::Tab)); // -> Duplicates
        app.handle_key(key(KeyCode::Enter)); // 聚焦
        app.handle_key(key(KeyCode::Char(' '))); // 标记 a
        assert!(app.marked.contains(&PathBuf::from("a")));
        app.handle_key(key(KeyCode::Down)); // 光标到 b
        app.handle_key(key(KeyCode::Char(' '))); // 标记 b
        assert!(app.marked.contains(&PathBuf::from("b")));
        assert_eq!(app.marked.len(), 2);
    }

    #[test]
    fn d_opens_plan_overlay_and_can_close() {
        let mut app = app_with(&[1]);
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.show_plan);
        app.handle_key(key(KeyCode::Down)); // 被吞
        assert!(app.show_plan);
        app.handle_key(key(KeyCode::Esc)); // 关闭
        assert!(!app.show_plan);
        assert!(!app.should_quit);
    }

    #[test]
    fn trash_delete_requests_after_single_y() {
        let mut app = app_with(&[10]);
        app.handle_key(key(KeyCode::Char(' '))); // 标记
        app.handle_key(key(KeyCode::Char('d'))); // 预览
        assert!(app.show_plan);
        app.handle_key(key(KeyCode::Enter)); // 进入确认
        assert!(app.confirm_delete);
        assert!(!app.show_plan);
        app.handle_key(key(KeyCode::Char('y'))); // trash 直接确认
        assert!(app.request_delete);
        assert!(!app.confirm_delete);
    }

    #[test]
    fn permanent_delete_needs_double_y() {
        let mut app = app_with(&[10]);
        app.mode = DeleteMode::Permanent;
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('y'))); // 第一次只 arm
        assert!(app.permanent_armed);
        assert!(!app.request_delete);
        app.handle_key(key(KeyCode::Char('y'))); // 第二次才执行
        assert!(app.request_delete);
    }

    #[test]
    fn delete_confirm_n_cancels() {
        let mut app = app_with(&[10]);
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('n')));
        assert!(!app.confirm_delete);
        assert!(!app.request_delete);
    }

    #[test]
    fn safe_deletion_keeps_one_copy_per_group() {
        let mut app = app_with(&[10, 10]);
        let p0 = app.rows[0].path.clone();
        let p1 = app.rows[1].path.clone();
        app.set_dups(vec![DuplicateGroup {
            hash: [0u8; 32],
            size: 10,
            paths: vec![p0.clone(), p1.clone()],
        }]);
        app.marked.insert(p0);
        app.marked.insert(p1);

        let safe = app.safe_deletion();
        assert_eq!(safe.targets.len(), 1, "整组标记仍须保留一份");
    }

    #[test]
    fn apply_outcome_shrinks_state_and_summarizes() {
        let mut app = app_with(&[10, 20, 30]);
        let victim = app.rows[0].path.clone(); // 最大那个
        let outcome = DeleteOutcome {
            deleted: vec![victim.clone()],
            freed: 30,
            failed: Vec::new(),
        };
        app.apply_delete_outcome(outcome);

        assert!(!app.result.entries.iter().any(|e| e.path == victim));
        assert_eq!(app.result.total_size, 30); // 10 + 20
        assert!(app.delete_result.is_some());
    }

    #[test]
    fn delete_result_overlay_dismissed_by_any_key() {
        let mut app = app_with(&[10]);
        app.delete_result = Some("done".to_string());
        app.handle_key(key(KeyCode::Down));
        assert!(app.delete_result.is_none());
    }

    #[test]
    fn delete_plan_sums_marked_files() {
        let mut app = app_with(&[10, 20, 30]);
        // 标记第一项（最大，30）
        let p0 = app.rows[0].path.clone();
        app.handle_key(key(KeyCode::Char(' ')));
        app.move_down();
        let p1 = app.rows[1].path.clone(); // 20
        app.handle_key(key(KeyCode::Char(' ')));

        let plan = app.delete_plan();
        assert_eq!(plan.targets.len(), 2);
        assert_eq!(plan.reclaimable, 50);
        assert!(plan.targets.iter().any(|e| e.path == p0));
        assert!(plan.targets.iter().any(|e| e.path == p1));
    }

    fn group(size: u64, paths: &[&str]) -> DuplicateGroup {
        DuplicateGroup {
            hash: [0u8; 32],
            size,
            paths: paths.iter().map(PathBuf::from).collect(),
        }
    }

    /// 进入 Duplicates 视图、含一个有 3 份副本的组。
    fn dup_app() -> App {
        let mut app = app_with(&[1]);
        app.set_dups(vec![group(10, &["a", "b", "c"])]);
        app.handle_key(key(KeyCode::Tab));
        app
    }

    #[test]
    fn enter_focuses_detail_then_scrolls_and_clamps() {
        let mut app = dup_app();
        assert!(!app.dup_focus);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.dup_focus);
        assert_eq!(app.detail_cursor, 0);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.detail_cursor, 1);
        app.handle_key(key(KeyCode::Down)); // 3 份 → 最大偏移 2
        assert_eq!(app.detail_cursor, 2);
        app.handle_key(key(KeyCode::Down)); // 钳制
        assert_eq!(app.detail_cursor, 2);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.detail_cursor, 1);

        app.handle_key(key(KeyCode::Backspace));
        assert!(!app.dup_focus);
    }

    #[test]
    fn group_nav_resets_detail_cursor() {
        let mut app = app_with(&[1]);
        app.set_dups(vec![
            group(10, &["a", "b", "c"]), // 可回收 20，排前
            group(5, &["d", "e"]),       // 可回收 5
        ]);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Down)); // 详情偏移 1
        assert_eq!(app.detail_cursor, 1);
        app.handle_key(key(KeyCode::Backspace)); // 退出聚焦
        app.handle_key(key(KeyCode::Down)); // 切到下一组
        assert_eq!(app.selected, 1);
        assert_eq!(app.detail_cursor, 0);
    }

    #[test]
    fn switching_view_resets_dup_focus() {
        let mut app = dup_app();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.dup_focus);
        app.handle_key(key(KeyCode::Tab)); // -> Sizes
        assert!(!app.dup_focus);
    }

    #[test]
    fn esc_exits_focus_before_quitting() {
        let mut app = dup_app();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Esc)); // 先退出聚焦
        assert!(!app.dup_focus);
        assert!(!app.confirm_quit);
        app.handle_key(key(KeyCode::Esc)); // 再按才弹退出确认
        assert!(app.confirm_quit);
        assert!(!app.should_quit);
        app.handle_key(key(KeyCode::Char('y'))); // 确认退出
        assert!(app.should_quit);
    }
}
