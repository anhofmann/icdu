//! Rendering. ncdu-style: a sorted list of the current directory's children, each with a size
//! bar, percentage of the parent, an optional item count, and a type indicator. Modal overlays (help, delete confirm)
//! are drawn on top.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{find_ci, indicator, App, Find, Modal, SortKey, TopFiles};
use crate::model::{Excluded, Node, NodeKind};

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// Human-readable size: powers of 1024, like ncdu.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["  B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{:>6} {}", bytes, UNITS[u])
    } else {
        format!("{:>6.1} {}", v, UNITS[u])
    }
}

/// Width of the item count column.
const ITEMS_WIDTH: usize = 5;

/// An item count in at most `ITEMS_WIDTH` columns: exact below 100,000, then compact (`123k`,
/// `4.1M`, `12.3M`, `123M`, `4.2G`). Compact forms round down, so a count never reads larger
/// than it is and `999,999` can't round up to a too-wide `1000k`.
fn fmt_items(n: u32) -> String {
    match n {
        0..=99_999 => n.to_string(),
        100_000..=999_999 => format!("{}k", n / 1_000),
        1_000_000..=99_999_999 => format!("{}.{}M", n / 1_000_000, n / 100_000 % 10),
        100_000_000..=999_999_999 => format!("{}M", n / 1_000_000),
        _ => format!("{}.{}G", n / 1_000_000_000, n / 100_000_000 % 10),
    }
}

/// The item count column's cell: the number of entries below a directory, blank for anything
/// else. An excluded directory is blank too: it was never read, so its count says nothing.
fn item_cell(n: &Node) -> String {
    if n.is_dir() && n.excluded == Excluded::No {
        fmt_items(n.items)
    } else {
        String::new()
    }
}

fn bar(frac: f64, width: usize) -> String {
    let filled = ((frac * width as f64).round() as usize).min(width);
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push('#');
    }
    for _ in filled..width {
        s.push(' ');
    }
    s
}

pub fn render(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(2), // header
        Constraint::Min(1),    // list
        Constraint::Length(1), // footer
    ])
    .split(f.area());

    render_header(f, app, chunks[0]);
    render_list(f, app, chunks[1]);
    render_footer(f, app, chunks[2]);

    match &app.modal {
        Modal::Help => render_help(f),
        Modal::ConfirmDelete(idx) => render_confirm(f, app, *idx),
        Modal::ConfirmExport => render_confirm_export(f, app),
        Modal::TopFiles(tf) => render_top_files(f, app, tf),
        Modal::Info(idx) => render_info(f, app, *idx),
        Modal::Find(find) => render_find(f, app, find),
        Modal::None => {}
    }
}

/// The info popup: tree data plus a fresh on-disk stat of the selected entry, label/value
/// rows. Read-only; any key closes it.
fn render_info(f: &mut Frame, app: &App, idx: usize) {
    let rows = app.info_pairs(idx);
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("  {k:<12} "), Style::default().fg(Color::Cyan)),
                Span::raw(v.as_str()),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));
    let width = 74u16;
    let height = wrapped_height(&lines, width - 2) + 2;
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Info ")),
        area,
    );
}

/// The top-files popup: largest regular files under the viewed directory, one row each, with
/// the selection highlighted. `j`/`k` move, `Enter` jumps to the file's parent, `q` closes.
fn render_top_files(f: &mut Frame, app: &App, tf: &TopFiles) {
    let width = f.area().width.saturating_sub(4).clamp(28, 96);
    let height = (tf.items.len() as u16 + 5)
        .min(f.area().height.saturating_sub(2))
        .max(7);
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Top files in {} ", app.tree.nodes[app.cur].name));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let metric = if app.disk_usage {
        "disk usage"
    } else {
        "apparent size"
    };
    let items: Vec<ListItem> = tf
        .items
        .iter()
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>10}  ", fmt_size(e.size)),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(e.path.clone()),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(tf.selected));
    f.render_stateful_widget(
        List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list_area,
        &mut state,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" j/k move · Enter go to file · q close   (sizes: {metric})"),
            Style::default().fg(Color::DarkGray),
        ))),
        hint_area,
    );
}

/// `1234567` → `1,234,567`, for match counts.
fn fmt_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Where to cut `path` so it fits in `width` columns when shown as `…` + the rest: the byte
/// index the shown tail starts at, 0 if the whole path fits. The cut is moved forward to the
/// next `/` when there is one, so the tail reads `…/deep/dir/name` and the name (where the
/// match is) stays visible.
fn truncate_left(path: &str, width: usize) -> usize {
    let len = path.chars().count();
    if len <= width {
        return 0;
    }
    // Keep the last `width - 1` chars; one column goes to the ellipsis.
    let skip = len - width.saturating_sub(1);
    let start = path.char_indices().nth(skip).map_or(path.len(), |(i, _)| i);
    match path[start..].find('/') {
        Some(rel) => start + rel,
        None => start,
    }
}

/// The search popup's title: what is searched, and once a query runs, the match count and the
/// progress of the pass. `scope` is the name of the directory a scoped search is limited to,
/// `shown` the number of hits kept (fewer than `total` once the cap is reached), `progress`
/// the percentage done while the pass is still running.
fn find_title(
    searching: bool,
    total: usize,
    shown: usize,
    scope: Option<&str>,
    progress: Option<u8>,
) -> String {
    let place = |whole: &str| match scope {
        Some(dir) => format!("under {dir}"),
        None => whole.to_string(),
    };
    if !searching {
        return format!(" Search {} ", place("the whole tree"));
    }
    let place = place("in the whole tree");
    let running = progress.map_or(String::new(), |pct| format!(" · searching… {pct}%"));
    if total > shown {
        let hint = if progress.is_some() {
            ""
        } else {
            " — refine the query"
        };
        format!(" {}+ matches {place}{running}{hint} ", fmt_count(shown))
    } else {
        let s = if total == 1 { "" } else { "es" };
        format!(" {} match{s} {place}{running} ", fmt_count(total))
    }
}

/// A directory name for a title: at most `width` columns, cut on the left, with a trailing `/`.
fn dir_label(name: &str, width: usize) -> String {
    let cut = truncate_left(name, width);
    let ellipsis = if cut > 0 { "…" } else { "" };
    let slash = if name.ends_with('/') { "" } else { "/" };
    format!("{ellipsis}{}{slash}", &name[cut..])
}

/// The search popup: the query line, one row per hit (path, size, the matched part of the name
/// highlighted), and the scope, match count and progress of the running pass in the title.
/// Paths are relative to the scan root, or to the viewed directory when the search is limited
/// to it. They are only built for the rows on screen.
fn render_find(f: &mut Frame, app: &App, find: &Find) {
    let width = f.area().width.saturating_sub(4).clamp(28, 120);
    let height = f.area().height.saturating_sub(2).max(7);
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);

    let busy = find.busy(&app.tree);
    let progress =
        busy.then(|| (find.cursor as f64 / app.tree.nodes.len().max(1) as f64 * 100.0) as u8);
    let scope = find.scope();
    let scope_name = scope.map(|dir| dir_label(&app.tree.nodes[dir].name, 30));
    let title = find_title(
        find.active.is_some(),
        find.total,
        find.hits.len(),
        scope_name.as_deref(),
        progress,
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [input_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " search ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {}▌", find.query)),
        ])),
        input_area,
    );

    let dim = Style::default().fg(Color::DarkGray);
    if find.hits.is_empty() {
        let msg = match (&find.active, scope) {
            (None, None) if find.query.is_empty() => {
                "  type to search entry names across the whole tree"
            }
            (None, Some(_)) if find.query.is_empty() => {
                "  type to search entry names below this directory"
            }
            (Some(q), None) if *q == find.query && !busy => "  no matches",
            (Some(q), Some(_)) if *q == find.query && !busy => {
                "  no matches below this directory (Tab searches the whole tree)"
            }
            _ => "",
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, dim))),
            list_area,
        );
    } else {
        let rows = (list_area.height as usize).max(1);
        let pos = find.selected_pos().unwrap_or(0);
        let offset = pos.saturating_sub(rows - 1);
        let q = find.active.as_deref().unwrap_or("");
        // Size column (10 + 2 spaces) and the rest for the path.
        let path_width = (list_area.width as usize).saturating_sub(12).max(8);
        let mark = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);

        let items: Vec<ListItem> = find.hits[offset..(offset + rows).min(find.hits.len())]
            .iter()
            .map(|&idx| {
                let n = &app.tree.nodes[idx];
                let mut path = app.tree.rel_path_from(idx, scope.unwrap_or(app.tree.root));
                // The name is the path's last component; locate the match inside it.
                let name_start = path.len() - n.name.len();
                let hit = find_ci(&n.name, q).map(|m| (name_start + m, name_start + m + q.len()));
                if n.is_dir() {
                    path.push('/');
                }
                let cut = truncate_left(&path, path_width);
                let mut spans = vec![Span::styled(
                    format!("{:>10}  ", fmt_size(app.size_of(idx))),
                    Style::default().fg(Color::Green),
                )];
                if cut > 0 {
                    spans.push(Span::styled("…", dim));
                }
                match hit {
                    Some((a, b)) if a >= cut => {
                        spans.push(Span::raw(path[cut..a].to_string()));
                        spans.push(Span::styled(path[a..b].to_string(), mark));
                        spans.push(Span::raw(path[b..].to_string()));
                    }
                    // No match found (can't happen for a live hit) or cut off: plain path.
                    _ => spans.push(Span::raw(path[cut..].to_string())),
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let mut state = ListState::default();
        state.select(find.selected_pos().map(|p| p - offset));
        f.render_stateful_widget(
            List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            list_area,
            &mut state,
        );
    }

    let metric = if app.disk_usage {
        "disk usage"
    } else {
        "apparent size"
    };
    let other = if scope.is_some() {
        "whole tree"
    } else {
        "this directory only"
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                " ↑/↓ PgUp/PgDn move · Enter go to entry · Tab {other} · Esc close   (sizes: {metric})"
            ),
            dim,
        ))),
        hint_area,
    );
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let path = app.tree.path_of(app.cur);
    let total = app.size_of(app.cur);
    let metric = if app.disk_usage {
        "disk usage"
    } else {
        "apparent"
    };
    let l1 = Line::from(vec![
        Span::styled(
            "icdu",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(path, Style::default().add_modifier(Modifier::BOLD)),
    ]);
    let own = app.own_size_of(app.cur);
    let l2 = Line::from(format!(
        "  total: {}  ({})   own: {}",
        fmt_size(total).trim_start(),
        metric,
        fmt_size(own).trim_start(),
    ));
    f.render_widget(Paragraph::new(vec![l1, l2]), area);
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let kids = app.sorted_children();
    let parent_total = app.size_of(app.cur).max(1);
    let max_child = kids
        .iter()
        .map(|&k| app.size_of(k))
        .max()
        .unwrap_or(1)
        .max(1);

    let mut items: Vec<ListItem> = Vec::with_capacity(kids.len());
    for &k in &kids {
        let n = &app.tree.nodes[k];
        let size = app.size_of(k);
        let pct = size as f64 / parent_total as f64 * 100.0;
        let frac = size as f64 / max_child as f64;

        let ind = indicator(n.kind, n.shared, n.excluded != Excluded::No, n.read_error);
        let name_style = if n.read_error || n.excluded != Excluded::No {
            Style::default().fg(Color::DarkGray)
        } else if n.shared {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        } else {
            match n.kind {
                NodeKind::Dir => Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
                NodeKind::Link => Style::default().fg(Color::Magenta),
                NodeKind::Other => Style::default().fg(Color::Yellow),
                NodeKind::File => Style::default(),
            }
        };

        // The subtree being deleted is locked: show a spinner and dim it. A subtree being
        // rescanned shows a spinner too, but stays styled normally (it's readable, not locked).
        let (name_style, suffix) = if app.deleting_idx() == Some(k) {
            (
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                format!("  [{} deleting…]", SPINNER[app.tick % SPINNER.len()]),
            )
        } else if app.refreshing_idx == Some(k) {
            (
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                format!("  [{} rescanning…]", SPINNER[app.tick % SPINNER.len()]),
            )
        } else {
            let suffix = match n.excluded {
                Excluded::OtherFs => "  <other filesystem>".to_string(),
                Excluded::Pattern => "  <excluded>".to_string(),
                Excluded::Age => "  <mtime filter>".to_string(),
                Excluded::No => String::new(),
            };
            (name_style, suffix)
        };

        let mut spans = vec![
            Span::styled(
                format!("{:>10} ", fmt_size(size)),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("[{}] ", bar(frac, 10)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(format!("{pct:>5.1}% ")),
        ];
        if app.show_item_count {
            spans.push(Span::raw(format!("{:>ITEMS_WIDTH$} ", item_cell(n))));
        }
        spans.push(Span::styled(format!("{ind}{}{suffix}", n.name), name_style));
        items.push(ListItem::new(Line::from(spans)));
    }

    let mut state = ListState::default();
    if let Some(sel) = app.selected {
        if let Some(pos) = kids.iter().position(|&k| k == sel) {
            state.select(Some(pos));
        }
    }

    let title = if app.tree.nodes[app.cur].children.is_empty() {
        if app.scanning {
            " reading… ".to_string()
        } else {
            " (empty) ".to_string()
        }
    } else {
        format!(" {} items ", kids.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(title),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(list, area, &mut state);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    // A transient status message wins (it carries warnings like the armed force-quit prompt).
    if let Some(msg) = &app.status {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ))),
            area,
        );
        return;
    }

    // Otherwise a running deletion is the most important thing to surface; show it but keep the
    // key hints so it's clear browsing still works.
    if let Some(didx) = app.deleting_idx() {
        let name = &app.tree.nodes[didx].name;
        let line = Line::from(vec![
            Span::styled(
                " j/k move  l enter  h up  q quit ",
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" │ "),
            Span::styled(
                format!("{} deleting {name} …", SPINNER[app.tick % SPINNER.len()]),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let status = if app.scanning {
        let rate = app
            .scan_rate()
            .map(|b| format!("  {}/s", fmt_size(b).trim_start()))
            .unwrap_or_default();
        format!(
            "{} scanning{rate}  {} dirs  {} files",
            SPINNER[app.tick % SPINNER.len()],
            app.tree.total_dirs,
            app.tree.total_files,
        )
    } else {
        format!(
            "done  {} dirs  {} files",
            app.tree.total_dirs, app.tree.total_files
        )
    };
    // An in-place rescan is worth surfacing even after the initial scan finished.
    let status = if let Some(idx) = app.refreshing_idx {
        format!(
            "{status}  {} rescanning {}…",
            SPINNER[app.tick % SPINNER.len()],
            app.tree.nodes[idx].name
        )
    } else {
        status
    };
    let sort = match app.sort {
        SortKey::Size => "size",
        SortKey::Name => "name",
    };
    let help =
        format!(" ? help  / search  d delete  s sort:{sort}  a usage  q quit   │   {status}");
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::Yellow),
        ))),
        area,
    );
}

/// Center a popup of the given size within `area`, clamped to fit.
fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(v);
    h
}

/// Rows a set of lines needs at a given inner width, accounting for wrapping.
fn wrapped_height(lines: &[Line], inner_width: u16) -> u16 {
    let iw = inner_width.max(1) as usize;
    lines
        .iter()
        .map(|l| l.width().max(1).div_ceil(iw) as u16)
        .sum()
}

fn render_help(f: &mut Frame) {
    let lines = vec![
        Line::from(vec![Span::styled(
            "icdu — keys",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from("  j / k, ↓ / ↑      move selection"),
        Line::from("  l / Enter / →     enter directory"),
        Line::from("  h / < / Backspace / ←  go up"),
        Line::from("  Home / End, G     jump to top / bottom"),
        Line::from("  PgUp / PgDn, C-d / C-u  page up / down"),
        Line::from("  s                 toggle sort (size ↔ name)"),
        Line::from(
            "  /                 search by name (Enter goes there, Tab: this directory only)",
        ),
        Line::from("                    in the search: C-u clear, C-w delete word, paste works"),
        Line::from("  a                 toggle apparent ↔ disk usage"),
        Line::from("  c                 toggle item count column (entries below each dir)"),
        Line::from("  d                 delete selected (confirm; needs full scan)"),
        Line::from("  o                 open selected in default app / file manager"),
        Line::from("  T                 largest files under this directory"),
        Line::from("  i                 show details of the selected entry"),
        Line::from("  r                 rescan this directory in place"),
        Line::from("  E                 export the tree to an ncdu-format JSON file"),
        Line::from("  x / !             jump to the next read-error entry"),
        Line::from("  b                 spawn $SHELL in this directory"),
        Line::from("  ?                 toggle this help"),
        Line::from("  q / Ctrl-C        quit (during a delete: press twice to abort it)"),
        Line::from("  Esc               close a popup"),
        Line::from("  C-l / C-z         redraw the screen / suspend to the shell"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  indicators: / dir   @ link   H hard-link dup   < excluded   ! error",
            Style::default().fg(Color::DarkGray),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  press any key to close",
            Style::default().add_modifier(Modifier::ITALIC),
        )]),
    ];
    let width = 74u16;
    let height = wrapped_height(&lines, width - 2) + 2;
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Help ")),
        area,
    );
}

fn render_confirm(f: &mut Frame, app: &App, idx: usize) {
    let n = &app.tree.nodes[idx];
    let path = app.tree.path_of(idx);
    // A directory goes with exactly what the scan listed inside it. Anything that appeared
    // since, or that the scan left out, stays (see `delete.rs`).
    let what = if n.is_dir() {
        format!(
            "directory and the {} scanned inside it ({})",
            match n.items {
                1 => "1 entry".to_string(),
                items => format!("{} entries", fmt_count(items as usize)),
            },
            fmt_size(app.size_of(idx)).trim_start()
        )
    } else {
        format!("file ({})", fmt_size(app.size_of(idx)).trim_start())
    };
    let mut lines = vec![
        Line::from(vec![Span::styled(
            "Delete?",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(format!("  {path}")),
        Line::from(format!("  This removes the {what} from disk.")),
    ];
    if let Some(note) = hard_link_note(app.confirm_hard_links, n.is_dir()) {
        lines.push(Line::from(format!("  {note}")));
    }
    lines.extend([
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  y",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" confirm    "),
            Span::styled("n / Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ]),
    ]);
    let width = 72.min(f.area().width.saturating_sub(4)).max(24);
    let height = wrapped_height(&lines, width - 2) + 2;
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(" Confirm deletion "),
        ),
        area,
    );
}

/// What the delete dialog says about hard-linked files in the selection: removing a link
/// frees the file's space only when it was the last one. Whether it is cannot be known here
/// (other links may lie outside the scanned directory), so the dialog does not promise the
/// space. `None` when there is nothing to say.
fn hard_link_note(hard_links: u32, is_dir: bool) -> Option<String> {
    let space = "space is only freed once";
    match (hard_links, is_dir) {
        (0, _) => None,
        (_, false) => Some(format!(
            "It has other hard links: its {space} the last one is gone."
        )),
        (1, true) => Some(format!(
            "1 file in it has other hard links: its {space} the last one is gone."
        )),
        (n, true) => Some(format!(
            "{} files in it have other hard links: their {space} the last ones are gone.",
            fmt_count(n as usize)
        )),
    }
}

/// Headline and the two explanatory lines of the export confirmation, for the number of
/// directories still unread. The dialog stays open while the scan goes on, so the number is
/// live and can reach zero under the user's eyes.
fn confirm_export_text(unread: usize) -> [String; 3] {
    let waiting = match unread {
        0 => {
            return [
                "Export the dump?".into(),
                "  The scan has finished in the meantime: every directory has been read.".into(),
                "  The dump will be complete.".into(),
            ]
        }
        1 => "1 directory has".to_string(),
        n => format!("{} directories have", fmt_count(n)),
    };
    [
        "Export a partial dump?".into(),
        format!("  The scan is still running: {waiting} not been read yet."),
        "  The dump marks them as unreadable (!) instead of showing them empty.".into(),
    ]
}

/// Asked before exporting while directories are still unread: the dump would otherwise look
/// like the result of a finished scan.
fn render_confirm_export(f: &mut Frame, app: &App) {
    let [headline, first, second] = confirm_export_text(app.tree.unread_dirs() as usize);
    let lines = vec![
        Line::from(vec![Span::styled(
            headline,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(first),
        Line::from(second),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  y",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" export now    "),
            Span::styled("n / Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ]),
    ];
    let width = 76.min(f.area().width.saturating_sub(4)).max(24);
    let height = wrapped_height(&lines, width - 2) + 2;
    let area = popup(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Confirm export "),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::{
        confirm_export_text, dir_label, find_title, fmt_count, fmt_items, hard_link_note,
        item_cell, truncate_left, ITEMS_WIDTH,
    };
    use crate::model::{Excluded, NodeKind, Tree};

    /// The delete dialog warns that deleting a hard link may free nothing.
    #[test]
    fn hard_link_note_only_appears_when_there_are_links() {
        assert_eq!(hard_link_note(0, false), None);
        assert_eq!(hard_link_note(0, true), None);
        let file = hard_link_note(1, false).unwrap();
        assert!(file.starts_with("It has other hard links"), "{file}");
        let one = hard_link_note(1, true).unwrap();
        assert!(one.starts_with("1 file in it has"), "{one}");
        let many = hard_link_note(1_500, true).unwrap();
        assert!(many.starts_with("1,500 files in it have"), "{many}");
        for note in [file, one, many] {
            assert!(note.contains("only freed once"), "{note}");
        }
    }

    /// The title always says what is being searched, so the scope is never silently on.
    #[test]
    fn find_title_names_the_scope() {
        assert_eq!(
            find_title(false, 0, 0, None, None),
            " Search the whole tree "
        );
        assert_eq!(
            find_title(false, 0, 0, Some("docs/"), None),
            " Search under docs/ "
        );
        assert_eq!(
            find_title(true, 1, 1, None, None),
            " 1 match in the whole tree "
        );
        assert_eq!(
            find_title(true, 1_234, 1_234, Some("docs/"), Some(40)),
            " 1,234 matches under docs/ · searching… 40% "
        );
        assert_eq!(
            find_title(true, 0, 0, Some("docs/"), None),
            " 0 matches under docs/ "
        );
        // Past the cap the count is a lower bound; the hint waits until the pass is done.
        assert_eq!(
            find_title(true, 25_000, 10_000, None, Some(7)),
            " 10,000+ matches in the whole tree · searching… 7% "
        );
        assert_eq!(
            find_title(true, 25_000, 10_000, Some("docs/"), None),
            " 10,000+ matches under docs/ — refine the query "
        );
    }

    #[test]
    fn dir_label_ends_in_one_slash_and_is_cut_on_the_left() {
        assert_eq!(dir_label("docs", 30), "docs/");
        assert_eq!(dir_label("/", 30), "/");
        assert_eq!(dir_label("/home/me/projects/thing", 12), "…/thing/");
        assert_eq!(dir_label("averyveryverylongname", 8), "…ongname/");
    }

    /// Exact while it fits, compact (rounded down) beyond, never wider than the column.
    #[test]
    fn fmt_items_fits_the_column() {
        assert_eq!(fmt_items(0), "0");
        assert_eq!(fmt_items(347), "347");
        assert_eq!(fmt_items(12_345), "12345");
        assert_eq!(fmt_items(99_999), "99999");
        assert_eq!(fmt_items(100_000), "100k");
        assert_eq!(fmt_items(999_999), "999k");
        assert_eq!(fmt_items(1_000_000), "1.0M");
        assert_eq!(fmt_items(4_190_000), "4.1M");
        assert_eq!(fmt_items(12_399_999), "12.3M");
        assert_eq!(fmt_items(99_999_999), "99.9M");
        assert_eq!(fmt_items(100_000_000), "100M");
        assert_eq!(fmt_items(999_999_999), "999M");
        assert_eq!(fmt_items(1_000_000_000), "1.0G");
        assert_eq!(fmt_items(u32::MAX), "4.2G");
        for n in (0..32).map(|s| 1u32 << s).chain([u32::MAX]) {
            for m in [n, n.saturating_sub(1), n / 10 * 9] {
                assert!(fmt_items(m).len() <= ITEMS_WIDTH, "{m} → {}", fmt_items(m));
            }
        }
    }

    /// Directories show their count (even 0); files, symlinks, special files and excluded
    /// directories stay blank.
    #[test]
    fn item_cell_is_blank_except_for_read_dirs() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let mut add = |parent, name: &str, kind, excluded| {
            t.import_child(
                parent,
                name.into(),
                kind,
                1,
                1,
                1,
                1,
                false,
                false,
                excluded,
                false,
            )
        };
        let d = add(0, "d", NodeKind::Dir, Excluded::No);
        let empty = add(0, "empty", NodeKind::Dir, Excluded::No);
        let ex = add(0, "ex", NodeKind::Dir, Excluded::Pattern);
        let f = add(d, "f", NodeKind::File, Excluded::No);
        let l = add(d, "l", NodeKind::Link, Excluded::No);
        let o = add(d, "o", NodeKind::Other, Excluded::No);
        assert_eq!(item_cell(&t.nodes[d]), "3");
        assert_eq!(item_cell(&t.nodes[empty]), "0");
        for idx in [ex, f, l, o] {
            assert_eq!(item_cell(&t.nodes[idx]), "", "{}", t.nodes[idx].name);
        }
    }

    #[test]
    fn fmt_count_groups_thousands() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }

    /// The export confirmation counts what is still unread, in the right grammatical number,
    /// and changes its question when the scan finishes while it is open.
    #[test]
    fn confirm_export_text_follows_the_scan() {
        let [headline, first, _] = confirm_export_text(1_500);
        assert_eq!(headline, "Export a partial dump?");
        assert!(first.contains("1,500 directories have not been read"));

        let [_, first, _] = confirm_export_text(1);
        assert!(first.contains("1 directory has not been read"));

        let [headline, first, second] = confirm_export_text(0);
        assert_eq!(headline, "Export the dump?");
        assert!(first.contains("finished"));
        assert!(second.contains("complete"));
    }

    /// Long paths lose their head, cut at a `/` so the tail (and the name) stays readable.
    #[test]
    fn truncate_left_keeps_the_tail() {
        assert_eq!(truncate_left("a/b/c.txt", 20), 0, "fits: no cut");
        let p = "very/long/prefix/deep/dir/match.txt";
        let cut = truncate_left(p, 20);
        assert_eq!(&p[cut..], "/deep/dir/match.txt");
        assert!(p[cut..].chars().count() < 20, "tail plus the ellipsis fits");
        // No `/` in the kept tail: cut mid-name, still within the width.
        let cut = truncate_left("x/averyveryverylongname", 10);
        assert_eq!(&"x/averyveryverylongname"[cut..], "ylongname");
        // Multi-byte names never cut inside a char.
        let p = "äöü/ßßßßßßßß/ëëë";
        let cut = truncate_left(p, 6);
        assert!(p.is_char_boundary(cut));
        assert_eq!(&p[cut..], "/ëëë");
    }
}
