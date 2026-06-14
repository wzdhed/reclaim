use super::app::{App, SortKey, View};
use crate::actions::DeleteMode;
use crate::report::human_size;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

/// 把 `App` 渲染出来：按视图画内容 + 底部提示，帮助打开时叠加浮层（只读借用 App）。
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    match app.view {
        View::Sizes => draw_sizes(frame, app, chunks[0]),
        View::Duplicates => draw_duplicates(frame, app, chunks[0]),
    }
    draw_footer(frame, app, chunks[1]);

    if app.show_help {
        draw_help(frame);
    }
    if app.show_plan {
        draw_delete_plan(frame, app);
    }
    if app.confirm_delete {
        draw_delete_confirm(frame, app);
    }
    if app.confirm_quit {
        draw_quit_confirm(frame);
    }
    if let Some(msg) = &app.delete_result {
        draw_delete_result(frame, msg);
    }
}

fn draw_sizes(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|r| {
            let name = r
                .path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            let slash = if r.is_dir { "/" } else { "" };
            let mark = if r.is_dir {
                "   "
            } else if app.marked.contains(&r.path) {
                "[x]"
            } else {
                "[ ]"
            };
            ListItem::new(format!("{mark} {:>10}  {name}{slash}", human_size(r.size)))
        })
        .collect();

    let sort_mark = match app.sort {
        SortKey::Size => "大小↓",
        SortKey::Name => "名称",
    };
    let scan_mark = if app.scanning {
        format!(" [扫描中 {}]", app.scanned_count)
    } else {
        String::new()
    };
    let title = format!(
        " Sizes [{sort_mark}] — {} ({}){scan_mark} ",
        app.cwd.display(),
        human_size(app.cwd_size())
    );

    let list = list_with_title(items, title);
    let mut state = ListState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_duplicates(frame: &mut Frame, app: &App, area: Rect) {
    let total_reclaimable: u64 = app.dups.iter().map(group_reclaimable).sum();
    let title = format!(
        " Duplicates — {} 组 (可回收 {}) ",
        app.dups.len(),
        human_size(total_reclaimable)
    );

    if app.scanning || !app.dups_ready || app.dups.is_empty() {
        let msg = if app.scanning {
            "扫描进行中，稍候检测重复…"
        } else if !app.dups_ready {
            "正在检测重复文件…"
        } else {
            "未发现重复文件"
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(title));
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    }

    // 上：每组一行的摘要（可平滑滚动、不会因某组副本过多而被整项跳过）；
    // 下：选中组的文件路径详情。聚焦浏览文件时把详情面板放大。
    let (list_c, detail_c) = if app.dup_focus {
        (Constraint::Length(5), Constraint::Min(1))
    } else {
        (Constraint::Min(1), Constraint::Length(8))
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([list_c, detail_c])
        .split(area);

    let items: Vec<ListItem> = app
        .dups
        .iter()
        .map(|g| {
            ListItem::new(format!(
                "{} × {} 份（可回收 {}）",
                human_size(g.size),
                g.paths.len(),
                human_size(group_reclaimable(g))
            ))
        })
        .collect();
    let list = list_with_title(items, title);
    let mut state = ListState::default();
    state.select(Some(app.selected));
    frame.render_stateful_widget(list, chunks[0], &mut state);

    // 选中组的文件路径：聚焦时可用 ↑/↓ 移动光标、Space 标记；带勾选框显示标记状态。
    let detail_items: Vec<ListItem> = match app.dups.get(app.selected) {
        Some(group) => group
            .paths
            .iter()
            .map(|p| {
                let mark = if app.marked.contains(p) { "[x]" } else { "[ ]" };
                ListItem::new(format!("{mark} {}", p.display()))
            })
            .collect(),
        None => Vec::new(),
    };
    let detail_title = if app.dup_focus {
        " 选中组的文件 [↑/↓ 移动 · Space 标记 · Esc 返回] "
    } else {
        " 选中组的文件 [Enter 浏览] "
    };
    let detail = list_with_title(detail_items, detail_title.to_string());
    let mut detail_state = ListState::default();
    if app.dup_focus {
        detail_state.select(Some(app.detail_cursor));
    }
    frame.render_stateful_widget(detail, chunks[1], &mut detail_state);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let base = match app.view {
        View::Sizes => {
            " [Sizes]  Enter 进入 · Space 标记 · d 预览 · Tab 视图 · s 排序 · ? 帮助 · q 退出 "
        }
        View::Duplicates if app.dup_focus => {
            " [Duplicates·浏览]  ↑/↓ 移动 · Space 标记 · d 预览 · Esc 返回 · q 退出 "
        }
        View::Duplicates => " [Duplicates]  Enter 看文件 · Tab 视图 · ↑/↓ 选组 · ? 帮助 · q 退出 ",
    };
    let hint = if app.marked.is_empty() {
        base.to_string()
    } else {
        format!("{base}· 已标记 {} ", app.marked.len())
    };
    frame.render_widget(Paragraph::new(hint), area);
}

fn draw_help(frame: &mut Frame) {
    let area = centered_rect(60, 50, frame.area());
    frame.render_widget(Clear, area);
    let text = Text::from(vec![
        Line::from("reclaim — 帮助"),
        Line::from(""),
        Line::from("  ↑/↓          上下移动"),
        Line::from("  Enter        Sizes: 进入目录 / Duplicates: 浏览该组文件"),
        Line::from("  Backspace    Sizes: 返回上级 / Duplicates: 退出浏览"),
        Line::from("  Tab          切换 Sizes / Duplicates"),
        Line::from("  s            切换排序（大小 / 名称）"),
        Line::from("  Space        标记 / 取消标记待删（仅文件）"),
        Line::from("  d            删除预览 →Enter 确认 →y 执行（默认入回收区）"),
        Line::from("  ?            显示 / 隐藏帮助"),
        Line::from("  q / Esc      退出（需确认）"),
        Line::from(""),
        Line::from("按 ? 或 Esc 关闭帮助"),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(" 帮助 "));
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_quit_confirm(frame: &mut Frame) {
    let area = centered_rect(40, 25, frame.area());
    frame.render_widget(Clear, area);
    let text = Text::from(vec![
        Line::from(""),
        Line::from("  确认退出 reclaim？"),
        Line::from(""),
        Line::from("  y / Enter  退出     n / Esc  取消"),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(" 退出确认 "));
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_delete_plan(frame: &mut Frame, app: &App) {
    let area = centered_rect(70, 60, frame.area());
    frame.render_widget(Clear, area);

    let plan = app.delete_plan();
    let mut lines: Vec<Line> = Vec::new();
    if plan.targets.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("  尚未标记任何文件（在列表里用 Space 标记）"));
    } else {
        for target in &plan.targets {
            lines.push(Line::from(format!(
                "  {:>10}  {}",
                human_size(target.size),
                target.path.display()
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "  将删除 {} 个文件，合计可释放 {}",
            plan.targets.len(),
            human_size(plan.reclaimable)
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "  Enter 进入删除确认 · Esc/d 取消  ",
    ));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(" 删除预览 "));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn draw_delete_confirm(frame: &mut Frame, app: &App) {
    let area = centered_rect(60, 35, frame.area());
    frame.render_widget(Clear, area);

    let plan = app.safe_deletion();
    let n = plan.targets.len();
    let kept = plan.kept_for_safety.len();

    let mut lines: Vec<Line> = vec![Line::from("")];
    match app.mode {
        DeleteMode::Trash => {
            lines.push(Line::from(format!("  将把 {n} 个文件移入回收区（可恢复）")));
            lines.push(Line::from(""));
            lines.push(Line::from("  y / Enter 确认      n / Esc 取消"));
        }
        DeleteMode::Permanent if app.permanent_armed => {
            lines.push(Line::from("  ⚠ 永久删除不可恢复！").red());
            lines.push(Line::from(format!("  再次按 y / Enter 永久删除这 {n} 个文件")).red());
            lines.push(Line::from(""));
            lines.push(Line::from("  按其它键取消"));
        }
        DeleteMode::Permanent => {
            lines.push(Line::from(format!("  将永久删除 {n} 个文件，不可恢复！")));
            lines.push(Line::from(""));
            lines.push(Line::from("  y / Enter 继续（需再确认一次）   n / Esc 取消"));
        }
    }
    if kept > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("  （重复组保护：保留 {kept} 份未删）")));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(" 删除确认 "));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn draw_delete_result(frame: &mut Frame, msg: &str) {
    let area = centered_rect(50, 22, frame.area());
    frame.render_widget(Clear, area);
    let text = Text::from(vec![
        Line::from(""),
        Line::from(format!("  {msg}")),
        Line::from(""),
        Line::from("  按任意键继续"),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(" 删除结果 "));
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn group_reclaimable(group: &crate::model::DuplicateGroup) -> u64 {
    group.size * (group.paths.len() as u64 - 1)
}

fn list_with_title(items: Vec<ListItem>, title: String) -> List {
    List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Line::from(title)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ")
}

/// 在 `area` 中取一个居中的、宽高各占百分比的矩形（用于帮助浮层）。
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}
