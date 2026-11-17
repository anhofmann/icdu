//! icdu — an ncdu-like disk usage explorer that is interactive while it scans.

mod app;
mod args;
mod delete;
mod dump;
mod glob;
mod matcher;
mod model;
mod natsort;
mod scan;
mod signals;
mod ui;

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;

use app::{App, InputMode, KeyAction};
use args::{Args, Feedback, Parsed};
use model::Tree;
use scan::Opts;
use signals::Signals;

/// What `--version` prints: the program name and its version, as ncdu does.
fn version_line() -> String {
    format!("icdu {}", env!("CARGO_PKG_VERSION"))
}

fn print_help() {
    println!(
        "icdu {} — interactive, SSD-optimized disk usage explorer (ncdu-compatible)

USAGE:
    icdu [OPTIONS] [PATH]

ARGS:
    PATH                     directory to scan (default: current directory)

OPTIONS:
{}

KEYS:
    j/k, down/up    move          l/Enter/right   enter directory
    h/</Backspace   go up         PgUp/PgDn,C-d/u page
    /               search names (Tab: whole tree / this directory)
    ] / [           next / previous match of the last search
    s / n / C / M   sort by size / name / items / mtime; again reverses
    t               directories first             T   largest files
    r               rescan this directory          a   apparent/disk
    d               delete (confirm; full scan)
    o               open          i               entry details
    E               export JSON   b               shell in this directory
    x / !           next error    ?               help    q  quit
    Home/End, G     top/bottom    c               item counts
    g               bar and percentage: both, none, one or the other
    e               show / hide dot files and excluded entries
    m               modification times (with -e)
    C-l             redraw        C-z             suspend (fg resumes)
    in the search:  C-u clear the line, C-w delete a word",
        env!("CARGO_PKG_VERSION"),
        args::options_help().trim_end()
    );
}

fn main() {
    // Usage errors exit with status 1, as in ncdu.
    let args = match args::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => *args,
        Ok(Parsed::Help) => {
            print_help();
            return;
        }
        Ok(Parsed::Version) => {
            println!("{}", version_line());
            return;
        }
        Err(e) => {
            eprintln!("icdu: {e}\nTry 'icdu --help'.");
            std::process::exit(1);
        }
    };

    // --- Export mode: produce a tree (by import or scan), write JSON, exit. No UI. ---
    if let Some(dest) = args.export.clone() {
        let tree = if let Some(src) = &args.import {
            // Convert/recompute: load a dump and write it back out.
            unwrap_or_die(load_import(src, args.extended), src)
        } else {
            let (tree, root, opts) = prepare_scan(&args);
            scan_to_completion(tree, root, opts, args.feedback_mode())
        };
        if let Err(e) = write_export(&tree, &dest) {
            eprintln!("icdu: export failed: {e}");
            std::process::exit(1);
        }
        // The dump is written and flushed. See `run_ui` for why not to return from here.
        std::process::exit(0);
    }

    // --- Import mode: load a dump, browse it, no scanning. ---
    if let Some(src) = &args.import {
        let tree = unwrap_or_die(load_import(src, args.extended), src);
        let mut app = App::new(tree, args.disk_usage, args.read_only > 0, false);
        apply_view_options(&mut app, &args);
        // A dump is not the local filesystem: nothing may act on its paths.
        app.imported = true;
        run_ui(&mut app, args.slow_ui);
    }

    let (tree, root, opts) = prepare_scan(&args);
    let mode = args.feedback_mode();
    if mode != Feedback::Browser {
        // --- `-0` / `-1`: scan first, with little or no feedback, then browse. ---
        let tree = scan_to_completion(tree, root, opts.clone(), mode);
        let mut app = App::new(tree, args.disk_usage, args.read_only > 0, false);
        apply_view_options(&mut app, &args);
        // The scan is over, but `r` can still read a directory again.
        app.set_scan_opts(opts);
        run_ui(&mut app, args.slow_ui);
    }

    // --- Interactive scan mode: browse while the scan runs. ---
    let scan = scan::start(root, opts.clone());
    let mut app = App::new(tree, args.disk_usage, args.read_only > 0, true);
    apply_view_options(&mut app, &args);
    app.attach_scan(scan, opts);

    run_ui(&mut app, args.slow_ui);
}

/// What the command line says about how the list looks: its columns and its order.
fn apply_view_options(app: &mut App, args: &Args) {
    app.show_item_count = args.show_item_count;
    app.show_mtime = args.show_mtime;
    app.show_hidden = args.show_hidden;
    app.show_graph = args.show_graph;
    app.show_percent = args.show_percent;
    app.graph_style = args.graph_style;
    if let Some((column, descending)) = args.sort {
        app.sort.column = column;
        app.sort.descending = descending;
    }
    app.sort.dirs_first = args.dirs_first;
    app.sort.natural = args.natsort;
}

fn unwrap_or_die<T>(res: std::io::Result<T>, ctx: &str) -> T {
    match res {
        Ok(v) => v,
        Err(e) => {
            eprintln!("icdu: {ctx}: {e}");
            std::process::exit(1);
        }
    }
}

/// Stat the root and build the initial tree + scan options, exiting on a bad path.
fn prepare_scan(args: &Args) -> (Tree, PathBuf, Opts) {
    let root = match std::fs::canonicalize(&args.path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("icdu: {}: {e}", args.path.display());
            std::process::exit(1);
        }
    };
    let meta = match std::fs::symlink_metadata(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("icdu: {}: {e}", root.display());
            std::process::exit(1);
        }
    };
    if !meta.is_dir() {
        eprintln!("icdu: {} is not a directory", root.display());
        std::process::exit(1);
    }
    // One path form everywhere: the tree's names, the scanner's queued jobs, and what we hand
    // to the OS all speak the plain (non-verbatim) form, so navigation focus, delete and open
    // all agree on the same string.
    let root_name = plain_path(&root);
    // A scan root whose path is not valid UTF-8 keeps its real bytes for everything that
    // touches the filesystem; `root_name` is then only its display form.
    let raw_root = root
        .to_str()
        .is_none()
        .then(|| Box::<[u8]>::from(root.as_os_str().as_bytes()));
    let root = match raw_root {
        Some(_) => root,
        None => PathBuf::from(&root_name),
    };
    let mut tree = Tree::new(
        root_name.into(),
        meta.len(),
        meta.blocks().saturating_mul(512),
        meta.dev(),
        meta.ino(),
    );
    if let Some(raw) = raw_root {
        tree.set_raw_name(tree.root, raw);
    }
    if args.extended {
        tree.set_extended();
        tree.set_ext(tree.root, scan::ext_of(&meta));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let opts = Opts {
        threads: args.threads,
        one_file_system: args.one_file_system,
        excludes: args.excludes.clone(),
        root_dev: meta.dev(),
        // Precompute the mtime cutoffs once, at scan start.
        older_than: args.older_than.map(|d| now - d as i64),
        newer_than: args.newer_than.map(|d| now - d as i64),
        extended: args.extended,
        exclude_caches: args.exclude_caches,
        exclude_kernfs: args.exclude_kernfs,
        follow_symlinks: args.follow_symlinks,
    };
    (tree, root, opts)
}

/// The scan root as a plain, user-visible path. `std::fs::canonicalize` returns a verbatim
/// `\\?\C:\…` path on Windows, which Win32 path normalization and the OS openers reject, so the
/// standard verbatim forms are reduced to their plain equivalent. Device paths have no plain
/// form and stay verbatim.
fn plain_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let reduced = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        Some(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        let is_drive_path = b.first().is_some_and(u8::is_ascii_alphabetic)
            && b.get(1) == Some(&b':')
            && b.get(2) == Some(&b'\\');
        is_drive_path.then(|| rest.to_string())
    } else {
        None
    };
    reduced.unwrap_or_else(|| s.into_owned())
}

/// Scan to the end without the browser. `Feedback::Line` shows one line of progress on
/// standard error, if that is a terminal; anything else shows nothing, so that a dump on
/// standard output or a cron job is not disturbed.
fn scan_to_completion(mut tree: Tree, root: PathBuf, opts: Opts, feedback: Feedback) -> Tree {
    use std::io::IsTerminal;
    let show = feedback == Feedback::Line && std::io::stderr().is_terminal();
    let scan = scan::start(root, opts);
    let mut shown = Instant::now();
    for batch in scan.events.iter() {
        tree.apply(batch);
        if show && shown.elapsed() >= Duration::from_millis(100) {
            eprint!(
                "\r{}",
                progress_line(tree.total_dirs, tree.total_files, false)
            );
            shown = Instant::now();
        }
    }
    tree.finish_scan();
    if show {
        eprintln!(
            "\r{}",
            progress_line(tree.total_dirs, tree.total_files, true)
        );
    }
    tree
}

/// The one line of progress of `-1`. It is padded so that a shorter line fully covers the
/// longer one it replaces.
fn progress_line(dirs: u64, files: u64, done: bool) -> String {
    let line = format!(
        "{} {} directories, {} files",
        if done { "scanned" } else { "scanning:" },
        ui::fmt_count(dirs as usize),
        ui::fmt_count(files as usize)
    );
    format!("{line:<60}")
}

/// Whether the screen is to be drawn in this turn of the event loop. Normally always. With
/// `-q`, while a scan runs, only every [`SLOW_REDRAW`]: over a slow remote connection every
/// redraw costs. What the user does is still shown at once.
fn draw_due(slow: bool, scanning: bool, acted: bool, since_last: Duration) -> bool {
    !slow || !scanning || acted || since_last >= SLOW_REDRAW
}

/// How often `-q` redraws the screen while a scan runs.
const SLOW_REDRAW: Duration = Duration::from_secs(2);

fn load_import(src: &str, extended: bool) -> std::io::Result<Tree> {
    if src == "-" {
        dump::import_with(BufReader::new(std::io::stdin().lock()), extended)
    } else {
        dump::import_with(BufReader::new(std::fs::File::open(src)?), extended)
    }
}

fn write_export(tree: &Tree, dest: &str) -> std::io::Result<()> {
    if dest == "-" {
        let stdout = std::io::stdout();
        let mut w = BufWriter::new(stdout.lock());
        dump::export(tree, &mut w)?;
        w.flush()
    } else {
        let mut w = BufWriter::new(std::fs::File::create(dest)?);
        dump::export(tree, &mut w)?;
        w.flush()
    }
}

/// Enter the TUI: raw mode and the alternate screen (ratatui), plus bracketed paste, so that
/// pasted text arrives as one event and not as a burst of key presses.
fn init_terminal() -> ratatui::DefaultTerminal {
    let terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    terminal
}

/// Leave the TUI and give the terminal back as it was.
fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
}

/// Run the TUI until the user quits or a signal ends it. Does not return: the process exits
/// from here, with status 0 after a normal quit and 1 after a terminal error.
fn run_ui(app: &mut App, slow: bool) -> ! {
    // ratatui's panic hook restores the screen but knows nothing of the paste mode. This hook
    // is installed first, so ratatui's wraps it and calls it after its own restore.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        default_hook(info);
    }));
    let signals = unwrap_or_die(Signals::install(), "cannot install signal handlers");
    let mut terminal = init_terminal();
    let result = run(&mut terminal, app, &signals, slow);
    restore_terminal();
    match result {
        // Asked to end by a signal: now that the terminal is back, let it take effect.
        Ok(Some(signal)) => signals::die_by(signal),
        // Leave at once. Returning would free the tree node by node, name by name, which on
        // a tree of many millions of entries keeps the prompt waiting, with the scanner
        // threads still reading the disk meanwhile. The OS takes it all back in one go.
        // Nothing is pending here: exports and deletions finish inside the loop.
        Ok(None) => std::process::exit(0),
        Err(e) => {
            eprintln!("icdu: {e}");
            std::process::exit(1);
        }
    }
}

/// Give the terminal back and stop the process (Ctrl-Z, or SIGTSTP from outside). Returns
/// once the process is continued, with the TUI set up again. The scan threads stop and
/// continue with the process.
fn suspend(terminal: &mut ratatui::DefaultTerminal, signals: &Signals) -> std::io::Result<()> {
    restore_terminal();
    signals::stop_process();
    // Running again. The SIGCONT that woke us needs no second answer.
    signals.take_cont();
    *terminal = init_terminal();
    terminal.clear()
}

/// How long a frame may spend taking scan results into the tree. With drawing on top, this
/// keeps a frame well under the 80 ms poll interval while leaving most of the time, when
/// there is a backlog, to working it off.
const SCAN_TIME_PER_FRAME: Duration = Duration::from_millis(12);

/// How long a frame may spend writing a dump that was started from the UI.
const EXPORT_TIME_PER_FRAME: Duration = Duration::from_millis(12);

/// The most input events handled between two frames.
const MAX_EVENTS_PER_FRAME: usize = 256;

/// Nodes the whole-tree search looks at per frame: a few milliseconds of work, so input stays
/// responsive while a pass over a 10M-entry tree finishes in well under a second.
const FIND_SLICE: usize = 200_000;

/// The event loop. Returns `Ok(None)` when the user quits, and `Ok(Some(signal))` when
/// SIGTERM or SIGHUP asked the process to end.
fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    signals: &Signals,
    slow: bool,
) -> std::io::Result<Option<i32>> {
    // For `-q`: when the screen was last drawn, and whether the user has acted since.
    let mut drawn: Option<Instant> = None;
    let mut acted = true;
    loop {
        // Signals only set flags (see `signals.rs`); this is where they take effect, at most
        // one poll interval later.
        if let Some(signal) = signals.terminated() {
            return Ok(Some(signal));
        }
        if signals.take_stop() {
            suspend(terminal, signals)?;
        } else if signals.take_cont() {
            // Stopped and continued behind our back (SIGSTOP cannot be caught): whatever
            // ran in between may have changed the terminal. Set it up again.
            restore_terminal();
            *terminal = init_terminal();
            terminal.clear()?;
        }

        // Take in what the scanners produced since the last frame, for a few milliseconds
        // at most: a scan that delivers faster than the tree can absorb must not starve input
        // handling and drawing. Stream completion (initial scan, subtree rescans) is folded
        // into the app state here too.
        app.poll_scans_until(usize::MAX, Some(Instant::now() + SCAN_TIME_PER_FRAME));

        // Advance the whole-tree search (if its popup is open) by one slice, right after the
        // new batches landed so live matches show up this frame.
        app.find_tick(Instant::now(), FIND_SLICE);

        // Fold in the result of any background deletion / system open.
        app.poll_delete();
        app.poll_open();
        // A directory the user is waiting for may have become complete.
        app.check_wait();
        // A dump being written gets its share of the frame too.
        app.poll_export(Some(Instant::now() + EXPORT_TIME_PER_FRAME));

        // A requested shell: fully tear down the TUI, hand the tty to the user's shell in the
        // target directory, and rebuild the UI when it exits. The scan keeps running in the
        // background; its events buffer until we're back.
        if let Some(dir) = app.take_shell_request() {
            app.status = run_shell(terminal, &dir);
            // Swallow any keys typed at the moment the shell exited.
            while event::poll(Duration::from_millis(0))? {
                let _ = event::read();
            }
        }

        // What the keys act on must be what is highlighted, from the first frame on.
        app.begin_frame(Instant::now());
        app.settle_selection();
        let since = drawn.map_or(Duration::MAX, |at| at.elapsed());
        if draw_due(slow, app.scanning, acted, since) {
            terminal.draw(|f| ui::render(f, app))?;
            drawn = Some(Instant::now());
            acted = false;
        }

        // Poll faster while scanning or deleting so updates/spinners animate smoothly. While a
        // search pass has work left, don't wait at all (input is still checked every slice),
        // and wake up in time for a debounced search query.
        let busy = app.scanning || app.is_deleting();
        let mut timeout = Duration::from_millis(if busy { 80 } else { 250 });
        if app.find_busy() || app.scan_backlog() || app.is_exporting() {
            // More to do right away: check for input, then carry on.
            timeout = Duration::ZERO;
        } else if let Some(due) = app.find_due() {
            timeout = timeout.min(due.saturating_duration_since(Instant::now()));
        }
        // Handle everything that is waiting before drawing again. With one event per frame,
        // a held key filled a queue faster than slow frames emptied it, and the cursor kept
        // moving after the key was released. The cap keeps the screen alive under a flood.
        let mut waiting = event::poll(timeout)?;
        let mut handled = 0;
        while waiting && handled < MAX_EVENTS_PER_FRAME {
            handled += 1;
            acted = true;
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // What the page keys move by: the rows of the list they act on. For the
                    // directory listing that is the terminal minus header, footer and the
                    // list's two border rows; the search popup has a list of its own.
                    let mode = app.input_mode();
                    let page = terminal
                        .size()
                        .map(|s| page_rows(mode, s.height))
                        .unwrap_or(20);
                    match decode_key(key.code, key.modifiers, mode, page) {
                        // The actions that need the terminal itself.
                        Some(KeyAction::Redraw) => terminal.clear()?,
                        Some(KeyAction::Suspend) => suspend(terminal, signals)?,
                        Some(action) => app.on_key(action),
                        None => {}
                    }
                }
                Event::Paste(text) => app.on_paste(&text),
                _ => {}
            }
            // Quitting and the shell take effect before anything typed after them.
            if app.quit || app.shell_requested() {
                break;
            }
            waiting = event::poll(Duration::ZERO)?;
        }
        app.tick = app.tick.wrapping_add(1);

        if app.quit {
            return Ok(None);
        }
    }
}

/// Suspend the TUI, run `$SHELL` interactively in `dir`, and rebuild the UI when it exits.
/// Returns a status line describing the outcome. This is the documented ratatui pattern for
/// handing the terminal to a child process; the child gets the real tty, keystrokes and all.
fn run_shell(terminal: &mut ratatui::DefaultTerminal, dir: &Path) -> Option<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
    let Some(shell) = shell else {
        return Some("no SHELL set — cannot spawn a shell".into());
    };
    restore_terminal();
    let outcome = match Command::new(&shell).current_dir(dir).status() {
        Ok(status) if status.success() => format!("shell exited (was in {})", dir.display()),
        Ok(status) => format!("shell exited with {status}"),
        Err(e) => format!("shell failed: {e}"),
    };
    *terminal = init_terminal();
    Some(outcome)
}

/// How many rows PgUp and PgDn move by in a terminal of the given height.
fn page_rows(mode: InputMode, terminal_height: u16) -> usize {
    match mode {
        InputMode::Find => ui::find_rows(terminal_height),
        InputMode::Normal | InputMode::Confirm => {
            (terminal_height.saturating_sub(5) as usize).max(1)
        }
    }
}

fn decode_key(
    code: KeyCode,
    mods: KeyModifiers,
    mode: InputMode,
    page: usize,
) -> Option<KeyAction> {
    // A letter with Ctrl, Alt or the like is not that letter: Ctrl-E must not export, Alt-D
    // must not delete, Ctrl-W must not type a `w`. Only the Ctrl combinations bound here do
    // anything, and none of them changes anything on disk. Shift is how capitals are typed
    // and does not count.
    let chord = KeyModifiers::CONTROL
        | KeyModifiers::ALT
        | KeyModifiers::SUPER
        | KeyModifiers::HYPER
        | KeyModifiers::META;
    if let (KeyCode::Char(c), true) = (code, mods.intersects(chord)) {
        if mods.intersection(chord) != KeyModifiers::CONTROL {
            return None;
        }
        let find = mode == InputMode::Find;
        return match c.to_ascii_lowercase() {
            // Ctrl-C quits even while a search query is being typed.
            'c' => Some(KeyAction::Quit),
            'l' => Some(KeyAction::Redraw),
            // In raw mode Ctrl-Z is a key press, not SIGTSTP.
            'z' => Some(KeyAction::Suspend),
            // Some terminals send ^H for the Backspace key.
            'h' if find => Some(KeyAction::FindBackspace),
            'h' => Some(KeyAction::Leave),
            'u' if find => Some(KeyAction::FindClear),
            'w' if find => Some(KeyAction::FindDeleteWord),
            // Vim-style half-page keys; a full page here.
            'd' if !find => Some(KeyAction::PageDown(page)),
            'u' if !find => Some(KeyAction::PageUp(page)),
            _ => None,
        };
    }
    // The search popup: letters go to the query, so only non-letter keys navigate.
    if mode == InputMode::Find {
        return Some(match code {
            KeyCode::Char(c) => KeyAction::FindChar(c),
            KeyCode::Backspace => KeyAction::FindBackspace,
            KeyCode::Down => KeyAction::Down,
            KeyCode::Up => KeyAction::Up,
            KeyCode::PageDown => KeyAction::PageDown(page),
            KeyCode::PageUp => KeyAction::PageUp(page),
            KeyCode::Home => KeyAction::Top,
            KeyCode::End => KeyAction::Bottom,
            KeyCode::Enter => KeyAction::Enter,
            KeyCode::Tab => KeyAction::FindScope,
            KeyCode::Esc => KeyAction::Cancel,
            _ => return None,
        });
    }
    Some(match code {
        KeyCode::Char('q') => KeyAction::Quit,
        // Esc closes popups and answers "no"; it never quits (as in ncdu).
        KeyCode::Esc => KeyAction::Cancel,
        KeyCode::Char('j') | KeyCode::Down => KeyAction::Down,
        KeyCode::Char('k') | KeyCode::Up => KeyAction::Up,
        KeyCode::PageDown => KeyAction::PageDown(page),
        KeyCode::PageUp => KeyAction::PageUp(page),
        KeyCode::Home => KeyAction::Top,
        KeyCode::Char('G') | KeyCode::End => KeyAction::Bottom,
        KeyCode::Char('l') | KeyCode::Enter | KeyCode::Right => KeyAction::Enter,
        KeyCode::Char('h') | KeyCode::Char('<') | KeyCode::Backspace | KeyCode::Left => {
            KeyAction::Leave
        }
        KeyCode::Char('s') => KeyAction::SortBySize,
        KeyCode::Char('C') => KeyAction::SortByItems,
        KeyCode::Char('M') => KeyAction::SortByMtime,
        KeyCode::Char('t') => KeyAction::ToggleDirsFirst,
        KeyCode::Char('a') => KeyAction::ToggleUsage,
        KeyCode::Char('c') => KeyAction::ToggleItemCount,
        KeyCode::Char('g') => KeyAction::CycleGraph,
        KeyCode::Char('u') => KeyAction::SharedColumn,
        KeyCode::Char('m') => KeyAction::ToggleMtime,
        KeyCode::Char('d') => KeyAction::Delete,
        KeyCode::Char('o') => KeyAction::Open,
        KeyCode::Char('T') => KeyAction::TopFiles,
        KeyCode::Char('r') => KeyAction::Refresh,
        KeyCode::Char('i') => KeyAction::Info,
        KeyCode::Char('E') => KeyAction::Export,
        KeyCode::Char('x') | KeyCode::Char('!') => KeyAction::NextError,
        KeyCode::Char('b') => KeyAction::Shell,
        KeyCode::Char('/') => KeyAction::Find,
        // Free in ncdu, whose `n` is "sort by name" and not vim's "next match".
        KeyCode::Char(']') => KeyAction::NextMatch,
        KeyCode::Char('[') => KeyAction::PrevMatch,
        KeyCode::Char('y') | KeyCode::Char('Y') => KeyAction::Confirm,
        // "No" where a question is open, and ncdu's "sort by name" in the list.
        KeyCode::Char('n') if mode == InputMode::Confirm => KeyAction::Cancel,
        KeyCode::Char('n') => KeyAction::SortByName,
        KeyCode::Char('N') => KeyAction::Cancel,
        KeyCode::Char('?') => KeyAction::Help,
        KeyCode::Char('e') => KeyAction::ToggleHidden,
        // The old search key, which is no key of ncdu's: say where the search went instead
        // of doing nothing.
        KeyCode::Char('f') => KeyAction::Moved("search is now on /"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use std::time::Duration;

    use super::{decode_key, draw_due, page_rows, plain_path, progress_line, version_line};
    use crate::app::{InputMode, KeyAction};

    #[test]
    fn verbatim_roots_reduce_to_plain_paths() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\Users\me")), r"C:\Users\me");
        assert_eq!(plain_path(Path::new(r"\\?\C:\")), r"C:\");
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\srv\share\dir")),
            r"\\srv\share\dir"
        );
        assert_eq!(plain_path(Path::new("/tmp/x")), "/tmp/x");
        // Not a drive path: reducing it would turn a device path into a bogus relative one.
        assert_eq!(
            plain_path(Path::new(r"\\?\.\PhysicalDrive0")),
            r"\\?\.\PhysicalDrive0"
        );
    }

    /// The program, the crate and the binary carry one name.
    #[test]
    fn the_program_is_called_icdu() {
        assert_eq!(env!("CARGO_PKG_NAME"), "icdu");
        assert_eq!(
            version_line(),
            format!("icdu {}", env!("CARGO_PKG_VERSION"))
        );
    }

    /// `update` used to be a subcommand that downloaded a release. It is a path like any other.
    #[test]
    fn update_is_an_ordinary_path() {
        let parsed = crate::args::parse(vec!["update".to_string()]);
        let Ok(crate::args::Parsed::Run(args)) = parsed else {
            panic!("{parsed:?}");
        };
        assert_eq!(args.path, PathBuf::from("update"));
        // What followed the old subcommand is not swallowed either.
        let err = crate::args::parse(vec!["update".to_string(), "--check".to_string()]);
        assert_eq!(err, Err("unknown option: --check".to_string()));
    }

    /// The repository this build's releases are published from.
    const REPO: &str = "anhofmann/icdu";

    /// The project this one started from. The README credits it with a link to the repository
    /// itself; nothing may be fetched from there.
    const ORIGIN: &str = "mayankasthana/rcdu";

    /// GitHub links in `text` that point anywhere but the release repo, as `host` + path.
    fn foreign_links(file: &str, text: &str) -> Vec<String> {
        let mut found = Vec::new();
        for host in ["github.com/", "githubusercontent.com/"] {
            for (at, _) in text.match_indices(host) {
                let rest = &text[at + host.len()..];
                if rest.starts_with('$') {
                    continue; // install.sh's own `$REPO` expansion
                }
                let path: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || "-_./".contains(*c))
                    .collect();
                // The README's credit link: the bare repository, never a path below it.
                if file == "README.md" && path == ORIGIN {
                    continue;
                }
                let repo = path.split('/').take(2).collect::<Vec<_>>().join("/");
                if repo != REPO {
                    found.push(format!("{host}{path}"));
                }
            }
        }
        found
    }

    /// `install.sh` and the README must fetch binaries from the repository that publishes this
    /// project's releases, never from another account (issue #2).
    #[test]
    fn install_paths_name_the_release_repo() {
        let install = include_str!("../install.sh");
        assert!(
            install.contains(&format!("REPO=\"{REPO}\"")),
            "install.sh REPO="
        );
        for (file, text) in [
            ("install.sh", install),
            ("README.md", include_str!("../README.md")),
        ] {
            assert_eq!(foreign_links(file, text), Vec::<String>::new(), "{file}");
        }
    }

    #[test]
    fn only_the_bare_origin_link_is_exempt() {
        let credit = "a fork of [rcdu](https://github.com/mayankasthana/rcdu) by";
        assert!(foreign_links("README.md", credit).is_empty());
        // A download path below the original repository is still foreign...
        let download = "curl https://github.com/mayankasthana/rcdu/releases/latest/download/x";
        assert_eq!(foreign_links("README.md", download).len(), 1);
        let raw = "https://raw.githubusercontent.com/mayankasthana/rcdu/main/install.sh";
        assert_eq!(foreign_links("README.md", raw).len(), 1);
        // ...and install.sh gets no exemption at all.
        assert_eq!(foreign_links("install.sh", credit).len(), 1);
    }

    /// Every key of the browser, with ncdu's meaning where ncdu has one. `e`, `t` and `g` are
    /// ncdu keys whose functions do not exist here yet: they only say where the function that
    /// used to sit on them went.
    #[test]
    fn decode_key_covers_every_key_in_normal_mode() {
        use KeyAction::*;
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let ch = KeyCode::Char;
        let table: &[(KeyCode, KeyModifiers, Option<KeyAction>)] = &[
            (ch('q'), none, Some(Quit)),
            (ch('c'), ctrl, Some(Quit)),
            (KeyCode::Esc, none, Some(Cancel)),
            (ch('j'), none, Some(Down)),
            (KeyCode::Down, none, Some(Down)),
            (ch('k'), none, Some(Up)),
            (KeyCode::Up, none, Some(Up)),
            (KeyCode::PageDown, none, Some(PageDown(10))),
            (KeyCode::PageUp, none, Some(PageUp(10))),
            (ch('d'), ctrl, Some(PageDown(10))),
            (ch('u'), ctrl, Some(PageUp(10))),
            (KeyCode::Home, none, Some(Top)),
            (KeyCode::End, none, Some(Bottom)),
            (ch('G'), none, Some(Bottom)),
            (ch('l'), none, Some(Enter)),
            (KeyCode::Enter, none, Some(Enter)),
            (KeyCode::Right, none, Some(Enter)),
            (ch('h'), none, Some(Leave)),
            (ch('<'), none, Some(Leave)),
            (KeyCode::Backspace, none, Some(Leave)),
            (KeyCode::Left, none, Some(Leave)),
            (ch('s'), none, Some(SortBySize)),
            (ch('C'), none, Some(SortByItems)),
            (ch('M'), none, Some(SortByMtime)),
            (ch('t'), none, Some(ToggleDirsFirst)),
            (ch('a'), none, Some(ToggleUsage)),
            (ch('c'), none, Some(ToggleItemCount)),
            (ch('g'), none, Some(CycleGraph)),
            (ch('u'), none, Some(SharedColumn)),
            (ch('m'), none, Some(ToggleMtime)),
            (ch('d'), none, Some(Delete)),
            (ch('o'), none, Some(Open)),
            (ch('T'), none, Some(TopFiles)),
            (ch('r'), none, Some(Refresh)),
            (ch('i'), none, Some(Info)),
            (ch('E'), none, Some(Export)),
            (ch('x'), none, Some(NextError)),
            (ch('!'), none, Some(NextError)),
            (ch('b'), none, Some(Shell)),
            (ch('/'), none, Some(Find)),
            (ch(']'), none, Some(NextMatch)),
            (ch('['), none, Some(PrevMatch)),
            (ch('y'), none, Some(Confirm)),
            (ch('Y'), none, Some(Confirm)),
            (ch('n'), none, Some(SortByName)),
            (ch('N'), none, Some(Cancel)),
            (ch('?'), none, Some(Help)),
            // Keys that used to do something else.
            (ch('e'), none, Some(ToggleHidden)),
            (ch('f'), none, Some(Moved("search is now on /"))),
            // Unbound.
            (ch('z'), none, None),
            (KeyCode::Tab, none, None),
            (KeyCode::F(1), none, None),
        ];
        for &(code, mods, want) in table {
            assert_eq!(
                decode_key(code, mods, InputMode::Normal, 10),
                want,
                "{code:?} with {mods:?}"
            );
        }
    }

    /// `-q` holds redraws back while a scan runs, and only then; a key press is always shown.
    #[test]
    fn slow_ui_draws_every_two_seconds_while_scanning() {
        let ms = Duration::from_millis;
        // Normally every turn of the loop draws.
        assert!(draw_due(false, true, false, ms(0)));
        // With -q, during a scan: only after two seconds...
        assert!(!draw_due(true, true, false, ms(80)));
        assert!(!draw_due(true, true, false, ms(1_999)));
        assert!(draw_due(true, true, false, ms(2_000)));
        assert!(
            draw_due(true, true, false, Duration::MAX),
            "the first frame"
        );
        // ...or when the user did something...
        assert!(draw_due(true, true, true, ms(80)));
        // ...and without a scan nothing is held back.
        assert!(draw_due(true, false, false, ms(80)));
    }

    /// The progress line of `-1` always covers the one before it.
    #[test]
    fn progress_line_keeps_its_width() {
        let running = progress_line(12_345, 6_789_012, false);
        let done = progress_line(12_345, 6_789_012, true);
        assert_eq!(
            running.trim_end(),
            "scanning: 12,345 directories, 6,789,012 files"
        );
        assert_eq!(
            done.trim_end(),
            "scanned 12,345 directories, 6,789,012 files"
        );
        assert_eq!(running.len(), done.len());
        assert_eq!(progress_line(0, 0, false).len(), running.len());
    }

    /// PgUp and PgDn move by the height of the list that has the focus. The search popup's
    /// list is shorter than the directory listing behind it.
    #[test]
    fn page_keys_move_by_the_rows_of_the_list_in_front() {
        assert_eq!(page_rows(InputMode::Normal, 24), 19);
        assert_eq!(page_rows(InputMode::Find, 24), 18);
        assert_eq!(page_rows(InputMode::Normal, 50), 45);
        assert_eq!(page_rows(InputMode::Find, 50), 44);
        // Tiny terminals still page by something.
        assert_eq!(page_rows(InputMode::Normal, 3), 1);
        assert_eq!(page_rows(InputMode::Find, 3), 1);
        assert_eq!(
            decode_key(
                KeyCode::PageDown,
                KeyModifiers::NONE,
                InputMode::Find,
                page_rows(InputMode::Find, 24)
            ),
            Some(KeyAction::PageDown(18))
        );
    }

    /// A letter with Ctrl or Alt is not that letter. Only the combinations that are bound do
    /// anything, and none of them deletes, writes a file or starts a rescan.
    #[test]
    fn modified_letters_do_not_act_as_plain_ones() {
        use KeyAction::*;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let dec = |c, mods| decode_key(KeyCode::Char(c), mods, InputMode::Normal, 10);

        // The bound combinations.
        assert_eq!(dec('c', ctrl), Some(Quit));
        assert_eq!(dec('d', ctrl), Some(PageDown(10)));
        assert_eq!(dec('u', ctrl), Some(PageUp(10)));
        assert_eq!(dec('l', ctrl), Some(Redraw));
        // Terminals that send ^H for the Backspace key.
        assert_eq!(dec('h', ctrl), Some(Leave));

        assert_eq!(dec('z', ctrl), Some(Suspend));

        let harmless = [Quit, PageDown(10), PageUp(10), Redraw, Leave, Suspend];
        let letters = ('a'..='z').chain('A'..='Z').chain("/?!<".chars());
        for c in letters {
            for mods in [
                ctrl,
                alt,
                ctrl | alt,
                KeyModifiers::SUPER,
                KeyModifiers::META,
            ] {
                match dec(c, mods) {
                    None => {}
                    Some(action) => assert!(
                        harmless.contains(&action),
                        "{c:?} with {mods:?} gives {action:?}"
                    ),
                }
            }
            // Alt is never bound.
            assert_eq!(dec(c, alt), None, "Alt-{c}");
        }
        // The examples from the bug report.
        for (c, mods) in [
            ('e', ctrl),
            ('E', ctrl),
            ('r', ctrl),
            ('d', alt),
            ('z', alt),
        ] {
            assert_eq!(dec(c, mods), None, "{c:?} with {mods:?}");
        }
        // Shift is how capitals are typed, not a modifier in this sense.
        assert_eq!(dec('G', KeyModifiers::SHIFT), Some(Bottom));
        assert_eq!(dec('E', KeyModifiers::SHIFT), Some(Export));
    }

    /// `n` is ncdu's "sort by name" in the list and "no" while a question is open. `y` and
    /// `N` mean the same everywhere.
    #[test]
    fn decode_key_reads_n_as_no_only_in_a_dialog() {
        let none = KeyModifiers::NONE;
        let key = |c, mode| decode_key(KeyCode::Char(c), none, mode, 10);
        assert_eq!(key('n', InputMode::Normal), Some(KeyAction::SortByName));
        assert_eq!(key('n', InputMode::Confirm), Some(KeyAction::Cancel));
        for mode in [InputMode::Normal, InputMode::Confirm] {
            assert_eq!(key('N', mode), Some(KeyAction::Cancel));
            assert_eq!(key('y', mode), Some(KeyAction::Confirm));
            assert_eq!(key('Y', mode), Some(KeyAction::Confirm));
        }
        // Every other key means in a dialog what it means in the list; the dialog decides
        // what to do with it.
        assert_eq!(key('s', InputMode::Confirm), Some(KeyAction::SortBySize));
        assert_eq!(key('q', InputMode::Confirm), Some(KeyAction::Quit));
    }

    /// In the search, a modified letter never reaches the query. Ctrl-U, Ctrl-W and Ctrl-H
    /// edit the line; Ctrl-L redraws in every mode.
    #[test]
    fn decode_key_edits_the_search_line() {
        use KeyAction::*;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let dec = |c, mods| decode_key(KeyCode::Char(c), mods, InputMode::Find, 10);
        assert_eq!(dec('u', ctrl), Some(FindClear));
        assert_eq!(dec('w', ctrl), Some(FindDeleteWord));
        assert_eq!(dec('h', ctrl), Some(FindBackspace));
        assert_eq!(dec('l', ctrl), Some(Redraw));
        assert_eq!(dec('c', ctrl), Some(Quit));
        assert_eq!(
            dec('z', ctrl),
            Some(Suspend),
            "Ctrl-Z suspends, not types z"
        );
        for (c, mods) in [('d', ctrl), ('a', ctrl), ('x', alt), ('u', alt)] {
            assert_eq!(dec(c, mods), None, "{c:?} with {mods:?}");
        }
        assert_eq!(dec('U', KeyModifiers::SHIFT), Some(FindChar('U')));
        assert_eq!(dec('u', KeyModifiers::NONE), Some(FindChar('u')));
    }

    /// `/` opens the search, and it is the only key that does; inside it letters (even `q`,
    /// `j`) go to the query, arrows still navigate, Tab switches the scope, Esc cancels and
    /// Ctrl-C still quits.
    #[test]
    fn decode_key_routes_find_mode() {
        let none = KeyModifiers::NONE;
        let dec = |code, mode| decode_key(code, none, mode, 10);
        assert!(matches!(
            dec(KeyCode::Char('/'), InputMode::Normal),
            Some(KeyAction::Find)
        ));
        assert!(matches!(
            dec(KeyCode::Tab, InputMode::Find),
            Some(KeyAction::FindScope)
        ));
        assert!(dec(KeyCode::Tab, InputMode::Normal).is_none());
        for c in ['q', 'j', 'f', '/'] {
            assert!(
                matches!(dec(KeyCode::Char(c), InputMode::Find), Some(KeyAction::FindChar(x)) if x == c),
                "{c} goes to the query"
            );
        }
        assert!(matches!(
            dec(KeyCode::Down, InputMode::Find),
            Some(KeyAction::Down)
        ));
        assert!(matches!(
            dec(KeyCode::PageUp, InputMode::Find),
            Some(KeyAction::PageUp(10))
        ));
        assert!(matches!(
            dec(KeyCode::Enter, InputMode::Find),
            Some(KeyAction::Enter)
        ));
        assert!(matches!(
            dec(KeyCode::Esc, InputMode::Find),
            Some(KeyAction::Cancel)
        ));
        assert!(matches!(
            dec(KeyCode::Backspace, InputMode::Find),
            Some(KeyAction::FindBackspace)
        ));
        assert!(matches!(
            decode_key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                InputMode::Find,
                10
            ),
            Some(KeyAction::Quit)
        ));
    }

    /// Plain `c` toggles the item count column; Ctrl-C still quits, and while a search is
    /// typed `c` goes to the query.
    #[test]
    fn decode_key_maps_c_to_item_count_toggle() {
        let none = KeyModifiers::NONE;
        assert!(matches!(
            decode_key(KeyCode::Char('c'), none, InputMode::Normal, 10),
            Some(KeyAction::ToggleItemCount)
        ));
        assert!(matches!(
            decode_key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                InputMode::Normal,
                10
            ),
            Some(KeyAction::Quit)
        ));
        assert!(matches!(
            decode_key(KeyCode::Char('c'), none, InputMode::Find, 10),
            Some(KeyAction::FindChar('c'))
        ));
    }
}
