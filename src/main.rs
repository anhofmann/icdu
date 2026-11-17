//! icdu — an ncdu-like disk usage explorer that is interactive while it scans.

mod app;
mod delete;
mod dump;
mod glob;
mod matcher;
mod model;
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
use model::Tree;
use scan::Opts;
use signals::Signals;

struct Args {
    path: PathBuf,
    threads: usize,
    disk_usage: bool,
    one_file_system: bool,
    read_only: bool,
    /// `--show-itemcount` / `--hide-itemcount`: initial state of the item count column.
    show_item_count: bool,
    excludes: Vec<String>,
    /// `--older-than DUR`: exclude entries not modified for this long (seconds).
    older_than: Option<u64>,
    /// `--newer-than DUR`: exclude entries modified within this window (seconds).
    newer_than: Option<u64>,
    /// `-o FILE`: scan headless and write ncdu JSON, then exit. `-` means stdout.
    export: Option<String>,
    /// `-f FILE`: load a tree from ncdu JSON instead of scanning. `-` means stdin.
    import: Option<String>,
}

/// Parse the command line (without the program name).
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut path: Option<PathBuf> = None;
    let mut threads = scan::default_threads();
    let mut disk_usage = true;
    let mut one_file_system = false;
    let mut read_only = false;
    let mut show_item_count = false;
    let mut excludes = Vec::new();
    let mut older_than = None;
    let mut newer_than = None;
    let mut export = None;
    let mut import = None;

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "-v" | "--version" => {
                println!("{}", version_line());
                std::process::exit(0);
            }
            "-t" | "--threads" => {
                let v = it.next().ok_or("--threads needs a value")?;
                threads = v
                    .parse::<usize>()
                    .map_err(|_| "--threads must be a number")?
                    .max(1);
            }
            "-a" | "--apparent-size" | "--apparent" => disk_usage = false,
            "-x" | "--one-file-system" => one_file_system = true,
            "-r" | "--read-only" => read_only = true,
            "--show-itemcount" => show_item_count = true,
            "--hide-itemcount" => show_item_count = false,
            "--exclude" => {
                excludes.push(it.next().ok_or("--exclude needs a pattern")?);
            }
            "--older-than" => {
                let d = it
                    .next()
                    .ok_or("--older-than needs a duration (e.g. 30d)")?;
                older_than = Some(parse_duration(&d)?);
            }
            "--newer-than" => {
                let d = it.next().ok_or("--newer-than needs a duration (e.g. 1y)")?;
                newer_than = Some(parse_duration(&d)?);
            }
            "-X" | "--exclude-from" => {
                let file = it.next().ok_or("--exclude-from needs a file")?;
                let content = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
                for line in content.lines() {
                    let line = line.trim();
                    if !line.is_empty() && !line.starts_with('#') {
                        excludes.push(line.to_string());
                    }
                }
            }
            "-o" | "--output" => export = Some(it.next().ok_or("-o needs a file (or -)")?),
            "-f" | "--file" => import = Some(it.next().ok_or("-f needs a file (or -)")?),
            // Bundled boolean short flags, e.g. `-rx`.
            other if other.starts_with('-') && !other.starts_with("--") && other.len() > 2 => {
                for ch in other[1..].chars() {
                    match ch {
                        'a' => disk_usage = false,
                        'x' => one_file_system = true,
                        'r' => read_only = true,
                        _ => return Err(format!("unknown flag -{ch} in {other}")),
                    }
                }
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option: {other}"));
            }
            other => path = Some(PathBuf::from(other)),
        }
    }

    Ok(Args {
        path: path.unwrap_or_else(|| PathBuf::from(".")),
        threads,
        disk_usage,
        one_file_system,
        read_only,
        show_item_count,
        excludes,
        older_than,
        newer_than,
        export,
        import,
    })
}

/// Parse a human duration like `45s`, `30m`, `12h`, `30d`, `2w`, `1y`. A bare number is days.
fn parse_duration(s: &str) -> Result<u64, String> {
    let (num, unit_secs) = match s.chars().last() {
        Some(c) if c.is_ascii_digit() => (s, 86_400u64),
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3_600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some('w') => (&s[..s.len() - 1], 604_800),
        Some('y') => (&s[..s.len() - 1], 31_536_000),
        _ => {
            return Err(format!(
                "invalid duration '{s}' (expected e.g. 45s, 30m, 12h, 30d, 2w, 1y)"
            ))
        }
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid duration '{s}'"))?;
    n.checked_mul(unit_secs)
        .ok_or_else(|| format!("duration '{s}' overflows"))
}

/// What `--version` prints: the program name and its version, as ncdu does.
fn version_line() -> String {
    format!("icdu {}", env!("CARGO_PKG_VERSION"))
}

fn print_help() {
    println!(
        "icdu {} — interactive, SSD-optimized disk usage explorer (ncdu-compatible)

USAGE:
    icdu [PATH] [OPTIONS]

ARGS:
    PATH                     directory to scan (default: current directory)

OPTIONS:
    -t, --threads N          scanner threads (default: cores*4, tuned for SSDs)
    -a, --apparent-size      show apparent size instead of on-disk usage
    -x, --one-file-system    do not cross filesystem boundaries
        --exclude PATTERN    exclude entries matching a glob (repeatable)
        --older-than DUR     exclude entries older than DUR (e.g. 30d, 2w, 1y)
        --newer-than DUR     exclude entries newer than DUR (surfaces stale data)
    -X, --exclude-from FILE  read exclude patterns from FILE (one per line)
    -r, --read-only          disable the delete feature
        --show-itemcount     show the item count column at start (toggle with c)
        --hide-itemcount     hide the item count column at start (default)
    -o, --output FILE        scan without UI and write ncdu-compatible JSON ('-' = stdout)
    -f, --file FILE          load a tree from ncdu JSON instead of scanning ('-' = stdin)
    -h, --help               show this help
    -V, --version            show version

KEYS:
    j/k, down/up    move          l/Enter/right   enter directory
    h/</Backspace   go up         PgUp/PgDn,C-d/u page
    /               search names (Tab: whole tree / this directory)
    s               toggle sort   T               largest files
    r               rescan this directory          a   apparent/disk
    d               delete (confirm; full scan)
    o               open          i               entry details
    E               export JSON   b               shell in this directory
    x / !           next error    ?               help    q  quit
    Home/End, G     top/bottom    c               item counts
    C-l             redraw        C-z             suspend (fg resumes)
    in the search:  C-u clear the line, C-w delete a word",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("icdu: {e}\nTry 'icdu --help'.");
            std::process::exit(2);
        }
    };

    // --- Export mode: produce a tree (by import or scan), write JSON, exit. No UI. ---
    if let Some(dest) = args.export.clone() {
        let tree = if let Some(src) = &args.import {
            // Convert/recompute: load a dump and write it back out.
            unwrap_or_die(load_import(src), src)
        } else {
            let (tree, root, opts) = prepare_scan(&args);
            scan_to_completion(tree, root, opts)
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
        let tree = unwrap_or_die(load_import(src), src);
        let mut app = App::new(tree, args.disk_usage, args.read_only, false);
        app.show_item_count = args.show_item_count;
        // A dump is not the local filesystem: nothing may act on its paths.
        app.imported = true;
        run_ui(&mut app);
    }

    // --- Interactive scan mode. ---
    let (tree, root, opts) = prepare_scan(&args);
    let scan = scan::start(root, opts.clone());
    let mut app = App::new(tree, args.disk_usage, args.read_only, true);
    app.show_item_count = args.show_item_count;
    app.attach_scan(scan, opts);

    run_ui(&mut app);
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

fn scan_to_completion(mut tree: Tree, root: PathBuf, opts: Opts) -> Tree {
    let scan = scan::start(root, opts);
    for batch in scan.events.iter() {
        tree.apply(batch);
    }
    tree.finish_scan();
    eprintln!(
        "icdu: scanned {} dirs, {} files",
        tree.total_dirs, tree.total_files
    );
    tree
}

fn load_import(src: &str) -> std::io::Result<Tree> {
    if src == "-" {
        dump::import(BufReader::new(std::io::stdin().lock()))
    } else {
        dump::import(BufReader::new(std::fs::File::open(src)?))
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
fn run_ui(app: &mut App) -> ! {
    // ratatui's panic hook restores the screen but knows nothing of the paste mode. This hook
    // is installed first, so ratatui's wraps it and calls it after its own restore.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        default_hook(info);
    }));
    let signals = unwrap_or_die(Signals::install(), "cannot install signal handlers");
    let mut terminal = init_terminal();
    let result = run(&mut terminal, app, &signals);
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
) -> std::io::Result<Option<i32>> {
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
        terminal.draw(|f| ui::render(f, app))?;

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
        InputMode::Normal => (terminal_height.saturating_sub(5) as usize).max(1),
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
        KeyCode::Char('s') => KeyAction::ToggleSort,
        KeyCode::Char('a') => KeyAction::ToggleUsage,
        KeyCode::Char('c') => KeyAction::ToggleItemCount,
        KeyCode::Char('d') => KeyAction::Delete,
        KeyCode::Char('o') => KeyAction::Open,
        KeyCode::Char('T') => KeyAction::TopFiles,
        KeyCode::Char('r') => KeyAction::Refresh,
        KeyCode::Char('i') => KeyAction::Info,
        KeyCode::Char('E') => KeyAction::Export,
        KeyCode::Char('x') | KeyCode::Char('!') => KeyAction::NextError,
        KeyCode::Char('b') => KeyAction::Shell,
        KeyCode::Char('/') => KeyAction::Find,
        KeyCode::Char('y') | KeyCode::Char('Y') => KeyAction::Confirm,
        KeyCode::Char('n') | KeyCode::Char('N') => KeyAction::Cancel,
        KeyCode::Char('?') => KeyAction::Help,
        // ncdu keys whose ncdu function is not implemented yet (hidden entries, directories
        // first, the graph column), and the old search key. Each used to do something else
        // here, so say where that went instead of doing nothing.
        KeyCode::Char('e') => KeyAction::Moved("export is now on E"),
        KeyCode::Char('t') => KeyAction::Moved("largest files are now on T"),
        KeyCode::Char('g') => KeyAction::Moved("jump to top is now on Home"),
        KeyCode::Char('f') => KeyAction::Moved("search is now on /"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::{
        decode_key, page_rows, parse_args, parse_duration, plain_path, version_line, Args,
    };
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
        let args = parse_args(vec!["update".to_string()]).unwrap();
        assert_eq!(args.path, PathBuf::from("update"));
        // What followed the old subcommand is not swallowed either.
        let Err(err) = parse_args(vec!["update".to_string(), "--check".to_string()]) else {
            panic!("--check is not an option");
        };
        assert!(err.contains("--check"), "{err}");
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
            (ch('s'), none, Some(ToggleSort)),
            (ch('a'), none, Some(ToggleUsage)),
            (ch('c'), none, Some(ToggleItemCount)),
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
            (ch('y'), none, Some(Confirm)),
            (ch('Y'), none, Some(Confirm)),
            (ch('n'), none, Some(Cancel)),
            (ch('N'), none, Some(Cancel)),
            (ch('?'), none, Some(Help)),
            // Keys that used to do something else.
            (ch('e'), none, Some(Moved("export is now on E"))),
            (ch('t'), none, Some(Moved("largest files are now on T"))),
            (ch('g'), none, Some(Moved("jump to top is now on Home"))),
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

    fn parse(args: &[&str]) -> Args {
        parse_args(args.iter().map(|a| a.to_string())).unwrap()
    }

    /// `--show-itemcount` / `--hide-itemcount` set the column's initial state (hidden by
    /// default); the last one given wins.
    #[test]
    fn itemcount_flags_set_the_initial_state() {
        assert!(!parse(&[]).show_item_count, "hidden by default");
        assert!(parse(&["--show-itemcount"]).show_item_count);
        assert!(!parse(&["--hide-itemcount"]).show_item_count);
        assert!(!parse(&["--show-itemcount", "--hide-itemcount"]).show_item_count);
        assert!(parse(&["--hide-itemcount", "--show-itemcount"]).show_item_count);
        let a = parse(&["/tmp", "--show-itemcount", "-rx"]);
        assert!(a.show_item_count && a.read_only && a.one_file_system);
        assert_eq!(a.path, PathBuf::from("/tmp"));
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

    #[test]
    fn parse_duration_understands_units() {
        assert_eq!(parse_duration("45s").unwrap(), 45);
        assert_eq!(parse_duration("30m").unwrap(), 1_800);
        assert_eq!(parse_duration("12h").unwrap(), 43_200);
        assert_eq!(parse_duration("30d").unwrap(), 2_592_000);
        assert_eq!(parse_duration("2w").unwrap(), 1_209_600);
        assert_eq!(parse_duration("1y").unwrap(), 31_536_000);
        assert_eq!(
            parse_duration("10").unwrap(),
            864_000,
            "bare numbers are days"
        );
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("1x").is_err());
    }
}
