//! Rendering. ncdu-style: a sorted list of the current directory's children, each with a size
//! bar, percentage of the parent, an optional item count, and a type indicator. Modal overlays (help, delete confirm)
//! are drawn on top.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{find_ci, indicator, scroll_offset, App, Find, Modal, SortKey, TopFiles};
use crate::model::{Excluded, Node, NodeIdx, NodeKind, Tree};

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
    let offset = scroll_offset(
        tf.offset.get(),
        Some(tf.selected),
        tf.items.len(),
        list_area.height as usize,
    );
    tf.offset.set(offset);
    let mut state = ListState::default()
        .with_offset(offset)
        .with_selected(Some(tf.selected));
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
        let offset = scroll_offset(
            find.offset.get(),
            find.selected_pos(),
            find.hits.len(),
            rows,
        );
        find.offset.set(offset);
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
                let incomplete = n.unscanned > 0;
                let mut spans = vec![Span::styled(
                    format!("{} ", size_cell(app.size_of(idx), incomplete)),
                    size_style(incomplete),
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
        // Only the rows on screen were built, so the highlight is relative to the offset.
        state.select(
            find.selected_pos()
                .and_then(|p| p.checked_sub(offset))
                .filter(|&p| p < rows),
        );
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

/// How far the listing of a directory has come. This is about the directory itself, not about
/// what lies below it and not about the scan as a whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Listing {
    /// Not read yet: its entries are still to come.
    Reading,
    /// Read to the end.
    Complete,
    /// Could not be read, or not to the end.
    Unreadable,
    /// Left out on purpose (`--exclude`, `-x`, the age filters); it will not be read.
    Excluded,
}

fn listing_state(tree: &Tree, dir: NodeIdx) -> Listing {
    let n = &tree.nodes[dir];
    if n.excluded != Excluded::No {
        Listing::Excluded
    } else if n.read_error {
        Listing::Unreadable
    } else if !n.scanned {
        Listing::Reading
    } else {
        Listing::Complete
    }
}

/// The title of the directory listing: how many entries it shows, and what an empty or
/// unfinished listing means.
fn list_title(state: Listing, items: usize) -> String {
    let count = match items {
        1 => "1 item".to_string(),
        n => format!("{} items", fmt_count(n)),
    };
    match (state, items) {
        (Listing::Excluded, _) => " (excluded, not scanned) ".into(),
        (Listing::Reading, 0) => " reading… ".into(),
        (Listing::Reading, _) => format!(" {count} · reading… "),
        (Listing::Unreadable, 0) => " (unreadable) ".into(),
        (Listing::Unreadable, _) => format!(" {count} · could not be read completely "),
        (Listing::Complete, 0) => " (empty) ".into(),
        (Listing::Complete, _) => format!(" {count} "),
    }
}

/// The size a full bar stands for: the largest size among the rows as last sorted and the
/// rows now on screen. Never 0, so it can be divided by.
fn bar_scale(sorted_max: u64, visible: impl Iterator<Item = u64>) -> u64 {
    visible.fold(sorted_max, u64::max).max(1)
}

/// The size column of a row, with the character that follows it. A directory whose subtree
/// is not fully read gets `…` there: its size is still growing. The mark is deliberately
/// not one of ncdu's flag characters.
fn size_cell(size: u64, incomplete: bool) -> String {
    let mark = if incomplete { '…' } else { ' ' };
    format!("{:>10}{mark}", fmt_size(size))
}

/// The style that goes with [`size_cell`]: dimmed while the size is not final.
fn size_style(incomplete: bool) -> Style {
    if incomplete {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Green)
    }
}

/// What the header says about the part of the viewed directory the scan has not reached.
fn left_to_read(unscanned: u32) -> Option<String> {
    match unscanned {
        0 => None,
        1 => Some("1 directory left to read".into()),
        n => Some(format!(
            "{} directories left to read",
            fmt_count(n as usize)
        )),
    }
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
    let mut l2 = vec![Span::raw(format!(
        "  total: {}  ({})   own: {}",
        fmt_size(total).trim_start(),
        metric,
        fmt_size(own).trim_start(),
    ))];
    // While the scan has not been everywhere below this directory, the total is a lower
    // bound. The count shrinks towards zero, which also shows that the scan is getting there.
    if let Some(left) = left_to_read(app.tree.nodes[app.cur].unscanned) {
        l2.push(Span::styled(
            format!("   {left}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(vec![l1, Line::from(l2)]), area);
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let kids = app.sorted_children();
    let parent_total = app.size_of(app.cur).max(1);

    // Where the list is scrolled to. Rows are built only for the part on screen: a directory
    // can have millions of entries, and a few dozen are visible.
    let pos = app
        .selected
        .and_then(|sel| kids.iter().position(|&k| k == sel));
    // The list has a border line above and below.
    let rows = area.height.saturating_sub(2) as usize;
    let offset = scroll_offset(app.list_offset.get(), pos, kids.len(), rows);
    app.list_offset.set(offset);
    let visible = &kids[offset.min(kids.len())..(offset + rows).min(kids.len())];
    // The bars are scaled by the largest row. That value comes with the cached order, which
    // sizes can outgrow while a scan runs; the rows on screen must never exceed the scale.
    let max_child = bar_scale(app.max_row_size(), visible.iter().map(|&k| app.size_of(k)));

    let mut items: Vec<ListItem> = Vec::with_capacity(visible.len());
    for &k in visible {
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

        let incomplete = n.unscanned > 0;
        let mut spans = vec![
            Span::styled(size_cell(size, incomplete), size_style(incomplete)),
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

    // The highlight is relative to the rows that were built.
    let mut state = ListState::default().with_selected(
        pos.and_then(|p| p.checked_sub(offset))
            .filter(|&p| p < visible.len()),
    );

    let title = list_title(listing_state(&app.tree, app.cur), kids.len());

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(title),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(list, area, &mut state);
}

/// What the running scans have done and have left to do, for the footer:
/// `scanning  12.3 MiB/s  1,234 dirs  56,789 files  321 dirs left`. The last figure is not a
/// percentage (the total is unknown until the scan is over), but it shrinks to nothing, which
/// tells the user the scan is getting there.
fn scan_progress(rate: Option<u64>, dirs: u64, files: u64, left: u32) -> String {
    let rate = rate
        .map(|bytes| format!("  {}/s", fmt_size(bytes).trim_start()))
        .unwrap_or_default();
    let left = match left {
        0 => String::new(),
        1 => "  1 dir left".to_string(),
        n => format!("  {} dirs left", fmt_count(n as usize)),
    };
    format!(
        "scanning{rate}  {} dirs  {} files{left}",
        fmt_count(dirs as usize),
        fmt_count(files as usize)
    )
}

/// How the footer line is shared when a status message and an activity (scan progress, a
/// running deletion) both want it: the columns for the message and for the activity. The
/// message comes first: the activity is shown only if both fit in full, and never at the
/// cost of a message that would be cut off.
fn footer_split(width: u16, message: usize, activity: usize) -> (u16, u16) {
    // One column before the message, two between it and the activity.
    let message = u16::try_from(message.saturating_add(1)).unwrap_or(u16::MAX);
    let activity = u16::try_from(activity.saturating_add(2)).unwrap_or(u16::MAX);
    if width >= message.saturating_add(activity) {
        (width - activity, activity)
    } else {
        (width, 0)
    }
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let spinner = SPINNER[app.tick % SPINNER.len()];
    // What is going on in the background, if anything. A running deletion is the most
    // important thing to surface.
    let activity: Option<(String, Style)> = if let Some(didx) = app.deleting_idx() {
        Some((
            format!("{spinner} deleting {} …", app.tree.nodes[didx].name),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if app.scanning {
        let mut text = format!(
            "{spinner} {}",
            scan_progress(
                app.scan_rate(),
                app.tree.total_dirs,
                app.tree.total_files,
                app.dirs_left(),
            )
        );
        // An in-place rescan is worth naming.
        if let Some(idx) = app.refreshing_idx {
            text.push_str(&format!("  rescanning {}…", app.tree.nodes[idx].name));
        }
        Some((text, Style::default().fg(Color::Yellow)))
    } else {
        None
    };

    // A status message takes the line (it carries warnings like the armed force-quit
    // prompt), but it shares it with the activity: a message from a background event, such
    // as "opened …", must not hide the progress of the scan for as long as no key is pressed.
    if let Some(msg) = &app.status {
        let (msg_width, activity_width) = match &activity {
            Some((text, _)) => footer_split(area.width, msg.chars().count(), text.chars().count()),
            None => (area.width, 0),
        };
        let [msg_area, activity_area] = Layout::horizontal([
            Constraint::Length(msg_width),
            Constraint::Length(activity_width),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(Line::from(format!(" {msg}")))
                .style(Style::default().fg(Color::Black).bg(Color::Yellow)),
            msg_area,
        );
        if let (Some((text, style)), true) = (activity, activity_width > 0) {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(format!("  {text}"), style))),
                activity_area,
            );
        }
        return;
    }

    // During a deletion the key hints stay, cut down, so it's clear browsing still works.
    if app.is_deleting() {
        let (text, style) = activity.unwrap_or_default();
        let line = Line::from(vec![
            Span::styled(
                " j/k move  l enter  h up  q quit ",
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" │ "),
            Span::styled(text, style),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let status = match activity {
        Some((text, _)) => text,
        None => format!(
            "done  {} dirs  {} files",
            fmt_count(app.tree.total_dirs as usize),
            fmt_count(app.tree.total_files as usize)
        ),
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
        bar_scale, confirm_export_text, dir_label, find_title, fmt_count, fmt_items, footer_split,
        hard_link_note, item_cell, left_to_read, list_title, listing_state, scan_progress,
        size_cell, truncate_left, Listing, ITEMS_WIDTH,
    };
    use crate::model::{Excluded, NodeKind, Tree};

    /// How long one key press plus one frame takes in a directory with a million entries.
    /// Not a pass/fail test: timing asserts are flaky on shared CI machines. Run it with
    ///
    /// ```sh
    /// cargo test --release frame_time_in_a_huge_directory -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn frame_time_in_a_huge_directory() {
        use crate::app::{App, KeyAction};
        use ratatui::{backend::TestBackend, Terminal};
        use std::time::Instant;

        const ENTRIES: usize = 1_000_000;
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        for i in 0..ENTRIES {
            // Sizes in no particular order, so the sort has work to do.
            let size = (i as u64).wrapping_mul(2_654_435_761) % 1_000_000_007;
            t.import_child(
                t.root,
                format!("entry-{i:07}.dat").into(),
                NodeKind::File,
                size,
                size,
                1,
                i as u64 + 2,
                false,
                false,
                Excluded::No,
                false,
            );
        }
        let mut app = App::new(t, true, false, false);
        let mut terminal = Terminal::new(TestBackend::new(120, 50)).unwrap();
        let mut frame = |app: &mut App| {
            app.settle_selection();
            terminal.draw(|f| super::render(f, app)).unwrap();
        };

        let start = Instant::now();
        frame(&mut app);
        println!("first frame:        {:>8.2?}", start.elapsed());

        let mut worst = std::time::Duration::ZERO;
        let start = Instant::now();
        const KEYS: u32 = 20;
        for _ in 0..KEYS {
            let one = Instant::now();
            app.on_key(KeyAction::Down);
            frame(&mut app);
            worst = worst.max(one.elapsed());
        }
        println!(
            "key press + frame:  {:>8.2?} on average, {:>8.2?} at worst",
            start.elapsed() / KEYS,
            worst
        );

        // The same at the far end of the list, where finding the highlighted row is slowest.
        app.on_key(KeyAction::Bottom);
        frame(&mut app);
        let start = Instant::now();
        for _ in 0..KEYS {
            app.on_key(KeyAction::Up);
            frame(&mut app);
        }
        println!(
            "at the end of it:   {:>8.2?} on average",
            start.elapsed() / KEYS
        );

        // A change of the sort key is the one thing that sorts on a key press.
        let start = Instant::now();
        app.on_key(KeyAction::ToggleSort);
        frame(&mut app);
        println!("sort by name:       {:>8.2?}", start.elapsed());
    }

    /// The four states a directory's listing can be in, each built the way a scan does.
    #[test]
    fn listing_state_tells_the_four_states_apart() {
        use crate::scan::{Batch, NewNode};
        let dir = |name: &str, id: u64, excluded| NewNode {
            name: name.into(),
            raw_name: None,
            kind: NodeKind::Dir,
            apparent: 0,
            disk: 0,
            dev: 1,
            ino: id,
            hlink: false,
            excluded,
            error: None,
            dir_id: (excluded == Excluded::No).then_some(id),
        };
        let mut t = Tree::new("/r".into(), 0, 0, 1, 1);
        assert_eq!(
            listing_state(&t, t.root),
            Listing::Reading,
            "nothing read yet"
        );
        t.apply(Batch {
            parent_id: 0,
            nodes: vec![
                dir("pending", 1, Excluded::No),
                dir("empty", 2, Excluded::No),
                dir("locked", 3, Excluded::No),
                dir("skipped", 4, Excluded::Pattern),
                dir("mount", 5, Excluded::OtherFs),
            ],
            error: None,
        });
        t.apply(Batch {
            parent_id: 2,
            nodes: Vec::new(),
            error: None,
        });
        t.apply(Batch {
            parent_id: 3,
            nodes: Vec::new(),
            error: Some("Permission denied".into()),
        });
        let state =
            |name: &str| listing_state(&t, t.nodes.iter().position(|n| n.name == name).unwrap());
        assert_eq!(listing_state(&t, t.root), Listing::Complete);
        assert_eq!(state("pending"), Listing::Reading);
        assert_eq!(state("empty"), Listing::Complete);
        assert_eq!(state("locked"), Listing::Unreadable);
        assert_eq!(state("skipped"), Listing::Excluded);
        assert_eq!(state("mount"), Listing::Excluded);

        // An imported tree was read in full when the dump was made.
        let imported = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        assert_eq!(listing_state(&imported, imported.root), Listing::Complete);
    }

    /// The title describes the directory on screen, not the scan as a whole: an empty
    /// directory that was read says so even while the scan is busy elsewhere.
    #[test]
    fn list_title_reflects_the_directory_itself() {
        assert_eq!(list_title(Listing::Reading, 0), " reading… ");
        assert_eq!(list_title(Listing::Complete, 0), " (empty) ");
        assert_eq!(list_title(Listing::Unreadable, 0), " (unreadable) ");
        assert_eq!(
            list_title(Listing::Excluded, 0),
            " (excluded, not scanned) "
        );
        assert_eq!(list_title(Listing::Complete, 1), " 1 item ");
        assert_eq!(list_title(Listing::Complete, 12_345), " 12,345 items ");
        // Entries are there, but the listing is not the whole story.
        assert_eq!(list_title(Listing::Reading, 40), " 40 items · reading… ");
        assert_eq!(
            list_title(Listing::Unreadable, 3),
            " 3 items · could not be read completely "
        );
    }

    /// A size that is still growing is marked, in the same width as one that is final.
    #[test]
    fn size_cell_marks_sizes_that_are_not_final() {
        assert_eq!(size_cell(1536, false), "   1.5 KiB ");
        assert_eq!(size_cell(1536, true), "   1.5 KiB…");
        assert_eq!(
            size_cell(0, true).chars().count(),
            size_cell(5 << 40, false).chars().count()
        );
    }

    /// No row may be larger than what a full bar stands for.
    #[test]
    fn bar_scale_covers_the_rows_on_screen() {
        assert_eq!(bar_scale(100, [10, 50].into_iter()), 100);
        assert_eq!(
            bar_scale(100, [10, 500].into_iter()),
            500,
            "a row outgrew it"
        );
        assert_eq!(bar_scale(0, std::iter::empty()), 1, "never zero");
    }

    /// The footer's account of a running scan.
    #[test]
    fn scan_progress_says_what_is_left() {
        assert_eq!(
            scan_progress(Some(12 << 20), 1_234, 56_789, 321),
            "scanning  12.0 MiB/s  1,234 dirs  56,789 files  321 dirs left"
        );
        assert_eq!(
            scan_progress(None, 1, 0, 1),
            "scanning  1 dirs  0 files  1 dir left"
        );
        // The last batches are in but the streams have not closed yet.
        assert_eq!(
            scan_progress(Some(0), 10, 20, 0),
            "scanning  0   B/s  10 dirs  20 files"
        );
    }

    /// A status message and the scan progress share the footer when both fit in full;
    /// otherwise the message has the line alone. Neither is ever cut off for the other.
    #[test]
    fn footer_split_never_cuts_the_message_for_the_progress() {
        // A 20-column message and a 60-column progress need 1 + 20 + 2 + 60 columns.
        assert_eq!(footer_split(120, 20, 60), (58, 62));
        assert_eq!(footer_split(83, 20, 60), (21, 62), "exactly enough");
        assert_eq!(
            footer_split(82, 20, 60),
            (82, 0),
            "one short: the message alone"
        );
        // A long message keeps the whole line even on a wide terminal.
        assert_eq!(footer_split(130, 72, 60), (130, 0));
        assert_eq!(footer_split(0, 0, 0), (0, 0));
        assert_eq!(footer_split(100, usize::MAX, 10), (100, 0));
        assert_eq!(footer_split(100, 10, usize::MAX), (100, 0));
    }

    #[test]
    fn left_to_read_counts_down_to_nothing() {
        assert_eq!(left_to_read(0), None);
        assert_eq!(left_to_read(1).as_deref(), Some("1 directory left to read"));
        assert_eq!(
            left_to_read(12_345).as_deref(),
            Some("12,345 directories left to read")
        );
    }

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
