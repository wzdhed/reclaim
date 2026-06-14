use crate::dedup::find_duplicates;
use crate::error::ScanError;
use crate::model::{DuplicateGroup, FileEntry};
use crate::scanner::{ScanItem, ScanOptions, scan_stream};
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;

/// 输入、后台扫描、去重三方汇流到主循环的统一事件。
pub enum AppEvent {
    Input(KeyEvent),
    Resize,
    ScanBatch(Vec<FileEntry>),
    ScanProgress(usize),
    ScanDone { warnings: Vec<ScanError> },
    DupsReady(Vec<DuplicateGroup>),
}

/// 每攒满这么多条目就推一批给 UI。
const BATCH: usize = 512;

/// 后台线程：阻塞读取终端输入，转成 `AppEvent` 发进 channel。
pub fn spawn_input_thread(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        loop {
            let app_event = match event::read() {
                // 只取按下事件，避免 Windows 上一次按键触发两次。
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => AppEvent::Input(key),
                Ok(Event::Resize(_, _)) => AppEvent::Resize,
                Ok(_) => continue,
                Err(_) => break,
            };
            if tx.send(app_event).is_err() {
                break; // 主循环已退出
            }
        }
    });
}

/// 后台线程：流式扫描，分批把条目与进度发给 UI，结束发 `ScanDone`。
pub fn spawn_scan_thread(root: PathBuf, options: ScanOptions, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let mut batch: Vec<FileEntry> = Vec::with_capacity(BATCH);
        let mut warnings = Vec::new();
        let mut count = 0usize;

        scan_stream(&root, &options, |item| match item {
            ScanItem::Entry(entry) => {
                count += 1;
                batch.push(entry);
                if batch.len() >= BATCH {
                    let _ = tx.send(AppEvent::ScanBatch(std::mem::take(&mut batch)));
                    let _ = tx.send(AppEvent::ScanProgress(count));
                }
            }
            ScanItem::Warning(warning) => warnings.push(warning),
        });

        if !batch.is_empty() {
            let _ = tx.send(AppEvent::ScanBatch(batch));
        }
        let _ = tx.send(AppEvent::ScanProgress(count));
        let _ = tx.send(AppEvent::ScanDone { warnings });
    });
}

/// 后台线程：对扫描所得条目做去重，完成后回填 `DupsReady`。
pub fn spawn_dedup_thread(entries: Vec<FileEntry>, threads: usize, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let (groups, _warnings) = find_duplicates(&entries, threads);
        let _ = tx.send(AppEvent::DupsReady(groups));
    });
}
