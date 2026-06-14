mod app;
mod event;
mod ui;

use crate::cli::Args;
use crate::scanner::ScanOptions;
use app::App;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use event::{AppEvent, spawn_dedup_thread, spawn_input_thread, spawn_scan_thread};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::sync::mpsc::channel;

/// RAII：进入时把终端切到 raw mode + 备用屏幕，`Drop` 时无条件恢复。
///
/// 任何退出路径（正常返回、`?` 提前返回、panic）都会触发 `Drop`，终端不会错乱。
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> crate::error::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Drop 不能传播错误；尽力恢复即可。
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// 启动交互式 TUI：后台线程流式扫描，主线程跑渲染/事件循环，边扫边显。
pub fn run_tui(args: &Args) -> crate::error::Result<()> {
    let threads = args.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    let options = ScanOptions {
        threads,
        min_size: args.min_size,
        include_hidden: args.hidden,
    };

    // 输入、扫描、去重三方汇流到这一个 channel；主循环单点消费。
    let (tx, rx) = channel::<AppEvent>();
    spawn_input_thread(tx.clone());
    spawn_scan_thread(args.path.clone(), options, tx.clone());

    let mut app = App::new(args.path.clone());
    app.mode = args.delete_mode;

    let _guard = TerminalGuard::new()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.draw(|frame| ui::draw(frame, &app))?;

    while !app.should_quit {
        let Ok(app_event) = rx.recv() else {
            break; // 所有发送端都没了
        };

        let mut scan_done = false;
        match app_event {
            AppEvent::Input(key) => app.handle_key(key),
            AppEvent::Resize => {}
            AppEvent::ScanBatch(entries) => app.add_entries(entries),
            AppEvent::ScanProgress(n) => app.set_progress(n),
            AppEvent::ScanDone { warnings } => {
                app.finish_scan(warnings);
                scan_done = true;
            }
            AppEvent::DupsReady(groups) => app.set_dups(groups),
        }

        // 用户已确认删除 → 在此（唯一写盘处）执行，再把结果回填 App。
        if app.request_delete {
            app.request_delete = false;
            let plan = app.safe_deletion();
            let outcome =
                crate::actions::execute_deletion(&plan.targets, app.mode, &app.result.root);
            app.apply_delete_outcome(outcome);
        }

        terminal.draw(|frame| ui::draw(frame, &app))?;

        // 扫描刚结束 → 在后台对已收集条目做去重，不阻塞 UI。
        if scan_done {
            spawn_dedup_thread(app.result.entries.clone(), threads, tx.clone());
        }
    }

    Ok(())
}
