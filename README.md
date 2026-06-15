# reclaim

一个用 Rust 编写的磁盘分析 / 去重 / 清理工具。给定一个目录，它**并行**扫描整棵目录树，
统计文件大小、按大小排序展示最占空间的项、找出内容完全相同的重复文件，并提供一个
交互式终端界面（TUI）来浏览结果、安全地删除文件。

## 构建

```
cargo build --release        
target/release/reclaim(.exe)
```

## 两种使用模式

- **一次性命令行**（默认）：扫描后打印结果即退出，**只读、不碰磁盘**。
- **交互式 TUI**（加 `--tui`）：全屏界面，可浏览/导航/标记/删除。

```
reclaim <PATH> [OPTIONS]
```

| 选项 | 说明 | 默认值 |
|------|------|--------|
| `<PATH>` | 要扫描的目录（必填） | — |
| `--top N` | 只显示最大的 N 项 | 20 |
| `--min-size SIZE` | 忽略小于该大小的文件，如 `1M`、`500K`、`2G` | 0 |
| `--dup` | 额外检测内容完全相同的重复文件 | 关闭 |
| `--format FMT` | 输出格式：`table` / `tree` / `json` | table |
| `--threads N` | 工作线程数 | CPU 核数 |
| `--hidden` | 包含隐藏文件（名字以 `.` 开头） | 关闭 |
| `--tui` | 启动交互式 TUI 界面 | 关闭 |
| `--delete-mode M` | TUI 删除方式：`trash`（回收区，可恢复）/ `permanent`（永久） | trash |

完整选项见 `reclaim --help`。

## 一次性命令行

```
reclaim . --top 10
reclaim D:\Study --threads 8 --min-size 1M
reclaim D:\Downloads --dup
reclaim . --format tree
reclaim . --format json > out.json
```

按大小降序输出（大小以人类可读形式展示）加总计：

```
    5.7 KB  src\scanner.rs
    2.4 KB  src\report.rs
    ...
total: 11.4 KB
```

启用 `--dup` 后追加重复组（按可回收空间降序）：

```
21 B × 3 份（可回收 42 B）
  .../a.txt
  .../b copy.txt
  .../sub/a-again.txt
总计可回收: 42 B
```

### 三种输出格式

- **table**（默认）：按大小降序列出 top-N 项 + 总计。
- **tree**：还原目录层级，每个目录展示其子孙文件的聚合大小，每层按大小降序、最多 `--top` 个子项。
- **json**：机器可读的单个文档（`root` / `total_size` / `entries` / `duplicates` / `warnings`），便于脚本处理。

单个文件/目录出错（无权限等）作为 `warning:` 打到 stderr，不中断整次扫描。

## 交互式 TUI（`--tui`）

```
reclaim D:\Downloads --tui
reclaim D:\Downloads --tui --threads 8 --delete-mode permanent
```

后台线程**边扫边显**：进界面即可用，标题实时显示「扫描中 N」，列表随扫描渐进填充；
扫完自动在后台做重复检测。两个视图 + 帮助浮层：

- **Sizes**：当前目录的子项列表，目录显示子孙文件聚合大小，可逐层进入。
- **Duplicates**：全树重复文件分组，可回收空间，可进组浏览每个副本。

### 按键

| 按键 | 作用 |
|------|------|
| ↑ / ↓ | 上下移动 |
| Enter | Sizes：进入选中目录；Duplicates：进入选中组浏览其文件 |
| Backspace | Sizes：返回上级；Duplicates：退出浏览 |
| Tab | 切换 Sizes / Duplicates |
| s | 切换排序（大小 / 名称） |
| Space | 标记 / 取消标记待删（仅文件，目录无效） |
| d | 删除预览 → Enter 进入确认 → y 执行 |
| ? | 显示 / 隐藏帮助 |
| q / Esc | 退出 |

### 删除

删除走严格流程，按绝不误删设计：

1. **dry-run 预览**：`d` 先列出将删文件的路径 + 大小 + 合计可释放，只看不删。
2. **显式确认**：Enter 进入确认框，默认不删；`trash` 模式按一次 `y`，`permanent` 模式需连按两次 y（第二次有红字警告）。
3. **重复组保护**：删除重复文件时**每组至少保留一份**，整组被标记时强制保留第一份。
4. **越界保护**：只删扫描根之内的文件，拒绝目录、拒绝根外路径，执行前再校验仍是普通文件。
5. **回收优先**：默认 `--delete-mode trash` 将文件移入 `<root>/.reclaim-trash/`（保留相对路径、可恢复，该目录以 `.` 开头默认不被扫描）；加了`--delete-mode permanent` 才真正删除。
6. **审计日志**：每删一个文件向 `<root>/reclaim-deleted.log` 追加「时间 + 大小 + 路径」。
7. **错误不崩**：单个文件删除失败收集进结果并在界面提示，不中断其余删除。

## 设计

### 并发扫描

用标准库实现：

- 共享工作队列 `Arc<Mutex<VecDeque<PathBuf>>>` + `Condvar`，根目录入队后启动 N 个 worker 线程；
- 每个 worker 从队列取一个目录 `std::fs::read_dir`：文件经 **mpsc channel** 发回，
  子目录塞回队列让其他线程接手；
- 用一个 `pending` 计数（队列中 + 处理中的目录数）跟踪进度，归零时唤醒所有 worker 退出；
- 提供流式入口 `scan_stream`：TUI 用它边扫边把结果分批送进事件 channel。

### 重复检测三级过滤（`--dup`）

逐级缩小候选，避免对所有文件做全量哈希：

1. **按大小分组**——大小独一无二的文件不可能重复，直接排除；
2. **头部哈希**——对同大小的候选并行算前 4 KB 的 blake3，头部不同的排除；
3. **全量哈希**——只对头部也相同的候选并行算全量 blake3，全量相同即内容重复。

第 2、3 级的哈希计算同样交给自建线程池并行执行。

## 单线程 vs 多线程计时对比

环境：32 逻辑核 CPU / Windows 11；扫描目标 `~/.rustup`（约 11.8 万个文件）；
`cargo build --release` 后对二进制计时，缓存预热后每个线程数取两次中较快一次的 wall-clock。

| 线程数 | 耗时 | 相对单线程加速 |
|-------:|-----:|---------------:|
| 1  | 0.532s | 1.00× |
| 2  | 0.351s | 1.52× |
| 8  | 0.322s | 1.65× |
| 32 | 0.245s | 2.17× |


并行带来约 2× 加速；继续加线程收益递减，因为目录遍历很快从 CPU 受限转为文件系统元数据调用受限。

## 模块结构

```
src/
├── main.rs      入口：解析参数 → 一次性扫描打印 / 或启动 TUI
├── lib.rs       对外导出各模块
├── cli.rs       clap 参数（Args / Format / --delete-mode）
├── model.rs     核心数据结构：FileEntry / EntryKind / ScanResult / DuplicateGroup
├── scanner.rs   并发扫描：工作队列 + 线程池 + channel（scan / scan_stream）
├── dedup.rs     重复检测：三级过滤 + 并行哈希
├── report.rs    trait Reporter + Table / Tree / Json 三种输出
├── actions.rs   删除：安全规划（保留一份/越界保护）+ 执行（回收区/永久）+ 审计日志
├── error.rs     统一错误 ScanError + Result 别名
└── tui/
    ├── mod.rs   run_tui + TerminalGuard（终端 setup/teardown）
    ├── app.rs   App 状态机 + 事件处理（纯逻辑，可单测）
    ├── event.rs 输入与后台扫描/去重事件汇流成一个 channel
    └── ui.rs    ratatui 渲染（只读借用 App）
```

依赖：`clap`（参数）、`blake3`（内容哈希）、`serde` + `serde_json`（JSON 输出）、
`ratatui` + `crossterm`（TUI）；dev：`tempfile`（集成测试）。

## 测试

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

状态机与删除等纯逻辑均有单元测试；删除相关测试全部在 `tempfile` 临时目录里跑，不碰真实数据。
集成测试在 `tests/` 下用临时目录验证扫描总量与重复检测。
