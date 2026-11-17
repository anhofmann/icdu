//! The command line.
//!
//! Written by hand (no clap), but driven by one table, [`FLAGS`]: the parser, `--help` and
//! the tests all read it, so a flag cannot be accepted without being documented or the other
//! way round. The accepted forms are those of ncdu, so that command lines written for ncdu
//! work unchanged:
//!
//! * short flags can be bundled, and the last one may take a value, attached or as the next
//!   argument: `-xo-`, `-xo file`, `-ofile`, `-1xo-`
//! * long flags take their value as `--long=value` or `--long value`
//! * `--` ends the options; everything after it is a path
//! * a flag given twice is counted (`-rr`), and of two flags that contradict each other the
//!   later one wins, which is what lets a command line override a config file

use std::path::PathBuf;

use crate::app::SortColumn;
use crate::scan;
use crate::ui::GraphStyle;

/// The settings a command line asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct Args {
    pub path: PathBuf,
    pub threads: usize,
    pub disk_usage: bool,
    pub one_file_system: bool,
    /// `--exclude-caches`: leave out directories tagged with a `CACHEDIR.TAG`.
    pub exclude_caches: bool,
    /// `--exclude-kernfs`: leave out Linux pseudo filesystems such as `/proc` and `/sys`.
    pub exclude_kernfs: bool,
    /// `-L`: count a symlink to a file as that file.
    pub follow_symlinks: bool,
    /// `--si`: sizes in powers of 1000 (kB, MB) instead of 1024 (KiB, MiB).
    pub si: bool,
    /// `--color`: `off` is `Some(false)`, `dark` and `dark-bg` are `Some(true)`. Without
    /// the flag colours are on unless `NO_COLOR` is set.
    pub color: Option<bool>,
    /// `--hide-hidden` starts without dot files and excluded entries in the list.
    pub show_hidden: bool,
    /// `--hide-graph` turns the size bar off.
    pub show_graph: bool,
    /// `--hide-percent` turns the percentage off.
    pub show_percent: bool,
    /// `--graph-style`: what the bar is drawn with.
    pub graph_style: GraphStyle,
    /// `--sort`: the column the list starts sorted by, and whether it runs downwards.
    /// Without it the list is sorted by the size it shows, largest first.
    pub sort: Option<(SortColumn, bool)>,
    /// `--group-directories-first`.
    pub dirs_first: bool,
    /// Names in natural order (`file2` before `file10`); `--disable-natsort` turns it off.
    pub natsort: bool,
    /// How many times `-r` was given. Once disables deleting.
    pub read_only: u8,
    /// The delete key works. `-r` and `--disable-delete` turn it off.
    pub can_delete: bool,
    /// The shell and the opener work. `-rr` and `--disable-shell` turn them off.
    pub can_shell: bool,
    /// Rescanning works. `--disable-refresh` turns it off.
    pub can_refresh: bool,
    /// The export key works. A second `-r` turns it off: it writes a file.
    pub can_export: bool,
    /// `--delete-command CMD`: delete by running CMD on the path.
    pub delete_command: Option<String>,
    /// `--confirm-quit`: ask before quitting.
    pub confirm_quit: bool,
    /// Ask before deleting. `--no-confirm-delete` turns it off.
    pub confirm_delete: bool,
    /// `--show-itemcount` / `--hide-itemcount`: initial state of the item count column.
    pub show_item_count: bool,
    pub excludes: Vec<String>,
    /// `--older-than DUR`: exclude entries not modified for this long (seconds).
    pub older_than: Option<u64>,
    /// `--newer-than DUR`: exclude entries modified within this window (seconds).
    pub newer_than: Option<u64>,
    /// `-o FILE`: scan headless and write ncdu JSON, then exit. `-` means stdout.
    pub export: Option<String>,
    /// `-f FILE`: load a tree from ncdu JSON instead of scanning. `-` means stdin.
    pub import: Option<String>,
    /// `-0`, `-1`, `-2`: how much to show while scanning; see [`Args::feedback_mode`].
    pub feedback: Option<u8>,
    /// `-q`: redraw only every two seconds while scanning, for slow remote connections.
    pub slow_ui: bool,
    /// `-e`: record owner, group, mode and modification time of every entry.
    pub extended: bool,
    /// `--show-mtime`: start with the modification time column. Only with `extended`.
    pub show_mtime: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            path: PathBuf::from("."),
            threads: scan::default_threads(),
            disk_usage: true,
            one_file_system: false,
            exclude_caches: false,
            exclude_kernfs: false,
            follow_symlinks: false,
            si: false,
            color: None,
            show_hidden: true,
            show_graph: true,
            show_percent: true,
            graph_style: GraphStyle::Hash,
            sort: None,
            dirs_first: false,
            natsort: true,
            read_only: 0,
            can_delete: true,
            can_shell: true,
            can_refresh: true,
            can_export: true,
            delete_command: None,
            confirm_quit: false,
            confirm_delete: true,
            show_item_count: false,
            excludes: Vec::new(),
            older_than: None,
            newer_than: None,
            export: None,
            import: None,
            feedback: None,
            slow_ui: false,
            extended: false,
            show_mtime: false,
        }
    }
}

/// How much is shown while a scan runs, as in ncdu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    /// `-0`: nothing.
    Silent,
    /// `-1`: one line of progress, on a terminal.
    Line,
    /// `-2`: the browser, live. Only without `-o`.
    Browser,
}

impl Args {
    /// The feedback mode these arguments ask for. Without `-0`, `-1` or `-2` it follows from
    /// where the output goes, as in ncdu: nothing when the dump goes to standard output (the
    /// reader of the pipe must not get progress mixed in, and scripts want silence), one line
    /// when it goes to a file, and the live browser when there is no dump to write. The
    /// browser cannot be asked for together with `-o`; `-2` then means one line.
    pub fn feedback_mode(&self) -> Feedback {
        let exporting = self.export.is_some();
        match (self.feedback, self.export.as_deref()) {
            (Some(0), _) => Feedback::Silent,
            (Some(1), _) => Feedback::Line,
            (Some(_), _) if exporting => Feedback::Line,
            (Some(_), _) => Feedback::Browser,
            (None, Some("-")) => Feedback::Silent,
            (None, Some(_)) => Feedback::Line,
            (None, None) => Feedback::Browser,
        }
    }
}

/// What a command line amounts to.
#[derive(Debug, PartialEq)]
pub enum Parsed {
    Run(Box<Args>),
    Help,
    Version,
}

/// What a flag does.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Help,
    Version,
    Threads,
    Apparent(bool),
    OneFileSystem(bool),
    Exclude,
    ExcludeFrom,
    OlderThan,
    NewerThan,
    IgnoreConfig,
    ReadOnly,
    Delete(bool),
    Shell(bool),
    Refresh(bool),
    ConfirmQuit(bool),
    ConfirmDelete(bool),
    DeleteCommand,
    ItemCount(bool),
    Output,
    Input,
    Feedback(u8),
    SlowUi(bool),
    Extended(bool),
    Mtime(bool),
    Caches(bool),
    Kernfs(bool),
    FollowSymlinks(bool),
    Si(bool),
    Color,
    /// A flag of ncdu's that icdu knows and deliberately does not have: the reason.
    Unsupported(&'static str),
    Hidden(bool),
    Graph(bool),
    Percent(bool),
    GraphStyle,
    SharedColumn,
    Sort,
    DirsFirst(bool),
    Natsort(bool),
}

/// One flag: how it is spelled, whether it takes a value, and what `--help` says about it.
pub struct Flag {
    /// Short spellings, without the dash.
    pub short: &'static [char],
    /// Long spellings, without the dashes. The first is the one `--help` shows.
    pub long: &'static [&'static str],
    /// The name of its value in `--help`, if it takes one.
    pub value: Option<&'static str>,
    /// The long name of the flag that undoes this one, if there is one.
    pub negation: Option<&'static str>,
    /// The text for `--help`. Empty for a flag that is accepted but not advertised, and for
    /// the second flag of a pair, which is shown with the first.
    pub help: &'static str,
    action: Action,
}

const fn flag(
    short: &'static [char],
    long: &'static [&'static str],
    value: Option<&'static str>,
    help: &'static str,
    action: Action,
) -> Flag {
    Flag {
        short,
        long,
        value,
        negation: None,
        help,
        action,
    }
}

/// Every flag there is.
pub const FLAGS: &[Flag] = &[
    flag(
        &['t'],
        &["threads"],
        Some("N"),
        "scanner threads (default: cores*4)",
        Action::Threads,
    ),
    Flag {
        negation: Some("disk-usage"),
        ..flag(
            &['a'],
            &["apparent-size", "apparent"],
            None,
            "show apparent sizes, or disk usage (default)",
            Action::Apparent(true),
        )
    },
    Flag {
        negation: Some("apparent-size"),
        ..flag(&[], &["disk-usage"], None, "", Action::Apparent(false))
    },
    Flag {
        negation: Some("cross-file-system"),
        ..flag(
            &['x'],
            &["one-file-system"],
            None,
            "stay on one filesystem, or cross (default)",
            Action::OneFileSystem(true),
        )
    },
    Flag {
        negation: Some("one-file-system"),
        ..flag(
            &[],
            &["cross-file-system"],
            None,
            "",
            Action::OneFileSystem(false),
        )
    },
    flag(
        &[],
        &["exclude"],
        Some("PATTERN"),
        "exclude entries matching a glob (repeatable)",
        Action::Exclude,
    ),
    flag(
        &['X'],
        &["exclude-from"],
        Some("FILE"),
        "read exclude patterns from FILE (one per line)",
        Action::ExcludeFrom,
    ),
    Flag {
        negation: Some("include-caches"),
        ..flag(
            &[],
            &["exclude-caches"],
            None,
            "leave out directories that have a CACHEDIR.TAG",
            Action::Caches(true),
        )
    },
    Flag {
        negation: Some("exclude-caches"),
        ..flag(&[], &["include-caches"], None, "", Action::Caches(false))
    },
    Flag {
        negation: Some("include-kernfs"),
        ..flag(
            &[],
            &["exclude-kernfs"],
            None,
            "leave out Linux pseudo filesystems (/proc, /sys)",
            Action::Kernfs(true),
        )
    },
    Flag {
        negation: Some("exclude-kernfs"),
        ..flag(&[], &["include-kernfs"], None, "", Action::Kernfs(false))
    },
    Flag {
        negation: Some("no-follow-symlinks"),
        ..flag(
            &['L'],
            &["follow-symlinks"],
            None,
            "count a symlink to a file as that file",
            Action::FollowSymlinks(true),
        )
    },
    Flag {
        negation: Some("follow-symlinks"),
        ..flag(
            &[],
            &["no-follow-symlinks"],
            None,
            "",
            Action::FollowSymlinks(false),
        )
    },
    flag(
        &[],
        &["older-than"],
        Some("DUR"),
        "exclude entries older than DUR (e.g. 30d, 2w, 1y)",
        Action::OlderThan,
    ),
    flag(
        &[],
        &["newer-than"],
        Some("DUR"),
        "exclude entries newer than DUR",
        Action::NewerThan,
    ),
    flag(
        &['r'],
        &["read-only"],
        None,
        "no deleting; given twice: no shell or export either",
        Action::ReadOnly,
    ),
    Flag {
        negation: Some("enable-delete"),
        ..flag(
            &[],
            &["disable-delete"],
            None,
            "turn the delete key off, or on (default)",
            Action::Delete(false),
        )
    },
    Flag {
        negation: Some("disable-delete"),
        ..flag(&[], &["enable-delete"], None, "", Action::Delete(true))
    },
    Flag {
        negation: Some("enable-shell"),
        ..flag(
            &[],
            &["disable-shell"],
            None,
            "turn shell and opener off, or on (default)",
            Action::Shell(false),
        )
    },
    Flag {
        negation: Some("disable-shell"),
        ..flag(&[], &["enable-shell"], None, "", Action::Shell(true))
    },
    Flag {
        negation: Some("enable-refresh"),
        ..flag(
            &[],
            &["disable-refresh"],
            None,
            "turn rescanning off, or on (default)",
            Action::Refresh(false),
        )
    },
    Flag {
        negation: Some("disable-refresh"),
        ..flag(&[], &["enable-refresh"], None, "", Action::Refresh(true))
    },
    flag(
        &[],
        &["delete-command"],
        Some("CMD"),
        "run CMD on the path instead of deleting it",
        Action::DeleteCommand,
    ),
    Flag {
        negation: Some("no-confirm-quit"),
        ..flag(
            &[],
            &["confirm-quit"],
            None,
            "ask before quitting, or not (default)",
            Action::ConfirmQuit(true),
        )
    },
    Flag {
        negation: Some("confirm-quit"),
        ..flag(
            &[],
            &["no-confirm-quit"],
            None,
            "",
            Action::ConfirmQuit(false),
        )
    },
    Flag {
        negation: Some("no-confirm-delete"),
        ..flag(
            &[],
            &["confirm-delete"],
            None,
            "ask before deleting (default), or not",
            Action::ConfirmDelete(true),
        )
    },
    Flag {
        negation: Some("confirm-delete"),
        ..flag(
            &[],
            &["no-confirm-delete"],
            None,
            "",
            Action::ConfirmDelete(false),
        )
    },
    Flag {
        negation: Some("no-si"),
        ..flag(
            &[],
            &["si"],
            None,
            "sizes in powers of 1000 (kB, MB), or of 1024",
            Action::Si(true),
        )
    },
    Flag {
        negation: Some("si"),
        ..flag(&[], &["no-si"], None, "", Action::Si(false))
    },
    flag(
        &[],
        &["color"],
        Some("SCHEME"),
        "off, dark or dark-bg (off if NO_COLOR is set)",
        Action::Color,
    ),
    Flag {
        negation: Some("hide-hidden"),
        ..flag(
            &[],
            &["show-hidden"],
            None,
            "list dot files and excluded entries, or not",
            Action::Hidden(true),
        )
    },
    Flag {
        negation: Some("show-hidden"),
        ..flag(&[], &["hide-hidden"], None, "", Action::Hidden(false))
    },
    Flag {
        negation: Some("hide-graph"),
        ..flag(
            &[],
            &["show-graph"],
            None,
            "show or hide the size bar (default: show)",
            Action::Graph(true),
        )
    },
    Flag {
        negation: Some("show-graph"),
        ..flag(&[], &["hide-graph"], None, "", Action::Graph(false))
    },
    Flag {
        negation: Some("hide-percent"),
        ..flag(
            &[],
            &["show-percent"],
            None,
            "show or hide the percentage (default: show)",
            Action::Percent(true),
        )
    },
    Flag {
        negation: Some("show-percent"),
        ..flag(&[], &["hide-percent"], None, "", Action::Percent(false))
    },
    flag(
        &[],
        &["graph-style"],
        Some("STYLE"),
        "the bar: hash, half-block or eighth-block",
        Action::GraphStyle,
    ),
    // Accepted for ncdu's sake, without effect and so without a line in the help.
    flag(
        &[],
        &["shared-column"],
        Some("COLUMN"),
        "",
        Action::SharedColumn,
    ),
    flag(
        &[],
        &["sort"],
        Some("COLUMN"),
        "disk-usage, name, apparent-size, itemcount, mtime",
        Action::Sort,
    ),
    Flag {
        negation: Some("no-group-directories-first"),
        ..flag(
            &[],
            &["group-directories-first"],
            None,
            "list directories before files, or not (default)",
            Action::DirsFirst(true),
        )
    },
    Flag {
        negation: Some("group-directories-first"),
        ..flag(
            &[],
            &["no-group-directories-first"],
            None,
            "",
            Action::DirsFirst(false),
        )
    },
    Flag {
        negation: Some("enable-natsort"),
        ..flag(
            &[],
            &["disable-natsort"],
            None,
            "order names byte by byte: file10 before file2",
            Action::Natsort(false),
        )
    },
    Flag {
        negation: Some("disable-natsort"),
        ..flag(&[], &["enable-natsort"], None, "", Action::Natsort(true))
    },
    Flag {
        negation: Some("hide-itemcount"),
        ..flag(
            &[],
            &["show-itemcount"],
            None,
            "show or hide the item count column (default: hide)",
            Action::ItemCount(true),
        )
    },
    Flag {
        negation: Some("show-itemcount"),
        ..flag(&[], &["hide-itemcount"], None, "", Action::ItemCount(false))
    },
    flag(
        &['o'],
        &["output"],
        Some("FILE"),
        "scan without UI, write ncdu JSON (- is stdout)",
        Action::Output,
    ),
    flag(
        &['f'],
        &["file"],
        Some("FILE"),
        "browse a tree from ncdu JSON (- is stdin)",
        Action::Input,
    ),
    // Short only, as in ncdu.
    flag(
        &['0'],
        &[],
        None,
        "no feedback while scanning (default with -o -)",
        Action::Feedback(0),
    ),
    flag(
        &['1'],
        &[],
        None,
        "one line of progress (default with -o FILE)",
        Action::Feedback(1),
    ),
    flag(
        &['2'],
        &[],
        None,
        "browse while scanning (default)",
        Action::Feedback(2),
    ),
    Flag {
        negation: Some("fast-ui-updates"),
        ..flag(
            &['q'],
            &["slow-ui-updates"],
            None,
            "update the screen every 2 s, or 10 times a second",
            Action::SlowUi(true),
        )
    },
    Flag {
        negation: Some("slow-ui-updates"),
        ..flag(&[], &["fast-ui-updates"], None, "", Action::SlowUi(false))
    },
    Flag {
        negation: Some("no-extended"),
        ..flag(
            &['e'],
            &["extended"],
            None,
            "record owner, group, mode and mtime of entries",
            Action::Extended(true),
        )
    },
    Flag {
        negation: Some("extended"),
        ..flag(&[], &["no-extended"], None, "", Action::Extended(false))
    },
    Flag {
        negation: Some("hide-mtime"),
        ..flag(
            &[],
            &["show-mtime"],
            None,
            "show or hide the mtime column (needs -e)",
            Action::Mtime(true),
        )
    },
    Flag {
        negation: Some("show-mtime"),
        ..flag(&[], &["hide-mtime"], None, "", Action::Mtime(false))
    },
    // ncdu 2's binary export and its compression are outside of what icdu does. These are
    // known, so that the answer is better than "unknown option", and not in the help.
    flag(
        &['O'],
        &[],
        Some("FILE"),
        "",
        Action::Unsupported("there is no binary export; -o FILE writes JSON"),
    ),
    flag(
        &['c'],
        &["compress"],
        None,
        "",
        Action::Unsupported(COMPRESS),
    ),
    flag(
        &[],
        &["no-compress"],
        None,
        "",
        Action::Unsupported(COMPRESS),
    ),
    flag(
        &[],
        &["compress-level"],
        Some("NUM"),
        "",
        Action::Unsupported(COMPRESS),
    ),
    flag(
        &[],
        &["export-block-size"],
        Some("KIB"),
        "",
        Action::Unsupported("it belongs to the binary export, which there is none of"),
    ),
    flag(
        &[],
        &["ignore-config"],
        None,
        "do not read the config files",
        Action::IgnoreConfig,
    ),
    flag(&['h'], &["help"], None, "show this help", Action::Help),
    flag(
        &['V', 'v'],
        &["version"],
        None,
        "show version",
        Action::Version,
    ),
];

/// Why the compression flags of ncdu 2 are refused.
const COMPRESS: &str = "dumps are written uncompressed; pipe one through zstd or gzip \
                        (icdu -o- DIR | zstd > FILE)";

/// The value of `--sort`: one of ncdu's column names, with `-asc` or `-desc` after it if the
/// direction is to differ from the column's own (names from a to z, the rest downwards).
fn parse_sort(value: &str) -> Result<(SortColumn, bool), String> {
    let (name, direction) = match value {
        v if v.ends_with("-desc") => (&v[..v.len() - 5], Some(true)),
        v if v.ends_with("-asc") => (&v[..v.len() - 4], Some(false)),
        v => (v, None),
    };
    let column = match name {
        "disk-usage" => SortColumn::DiskUsage,
        "apparent-size" => SortColumn::ApparentSize,
        "name" => SortColumn::Name,
        "itemcount" => SortColumn::Items,
        "mtime" => SortColumn::Mtime,
        _ => {
            return Err(format!(
                "unknown --sort column: {value} (disk-usage, name, apparent-size, itemcount \
                 or mtime, optionally followed by -asc or -desc)"
            ))
        }
    };
    Ok((column, direction.unwrap_or(column.descends_by_default())))
}

/// Parse the command line (without the program name).
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut parsed = Args::default();
    let mut path = None;
    match read_args(&mut parsed, &mut path, args.into_iter(), &From::CommandLine)? {
        Some(stop) => Ok(stop),
        None => Ok(finish(parsed, path)),
    }
}

fn finish(mut parsed: Args, path: Option<String>) -> Parsed {
    if let Some(path) = path {
        parsed.path = PathBuf::from(path);
    }
    Parsed::Run(Box::new(parsed))
}

/// Where arguments are being read from. A config file is not a command line: it sets
/// defaults and cannot name what to scan or what to do.
enum From<'a> {
    CommandLine,
    /// A line of a config file. `home` is what a leading `~/` in a file name stands for
    /// there (on a command line the shell has seen to that).
    Config {
        home: Option<&'a str>,
    },
}

/// Apply `args` to `parsed`, in order. Returns `Some` for the flags that end parsing
/// (`--help`, `--version`). The directory to scan, if one is named, goes to `path`.
fn read_args(
    parsed: &mut Args,
    path: &mut Option<String>,
    mut args: impl Iterator<Item = String>,
    from: &From,
) -> Result<Option<Parsed>, String> {
    let in_config = matches!(from, From::Config { .. });
    // What a config file may not say, and the value of a flag as a config file means it.
    let check = |flag: &Flag, shown: &str, value: Option<String>| match from {
        From::CommandLine => Ok(value),
        From::Config { home } => {
            let per_run = matches!(
                flag.action,
                Action::Help | Action::Version | Action::Output | Action::Input
            );
            if per_run || flag.action == Action::IgnoreConfig {
                return Err(format!("{shown} cannot be set in a config file"));
            }
            Ok(match (flag.action, value, home) {
                (Action::ExcludeFrom, Some(file), Some(home)) => Some(expand_home(&file, home)),
                (_, value, _) => value,
            })
        }
    };
    let mut only_paths = false;
    while let Some(arg) = args.next() {
        // A lone `-` is not an option; neither is anything after `--`.
        if only_paths || arg == "-" || !arg.starts_with('-') {
            if in_config {
                return Err(format!("not an option: {arg}"));
            }
            if let Some(first) = &path {
                return Err(format!("more than one directory given: {first} and {arg}"));
            }
            *path = Some(arg);
        } else if arg == "--" {
            only_paths = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            let flag = FLAGS
                .iter()
                .find(|f| f.long.contains(&name))
                .ok_or_else(|| format!("unknown option: --{name}"))?;
            let shown = format!("--{name}");
            let value = match (flag.value, attached) {
                (Some(_), Some(value)) => Some(value),
                (Some(_), None) => Some(args.next().ok_or_else(|| needs_value(flag, &shown))?),
                (None, Some(_)) => return Err(format!("{shown} does not take a value")),
                (None, None) => None,
            };
            let value = check(flag, &shown, value)?;
            match apply(flag.action, value, &shown, parsed)? {
                Some(stop) => return Ok(Some(stop)),
                None => continue,
            }
        } else {
            // A bundle of short flags. The first one that takes a value takes the rest of
            // the bundle as that value, or the next argument if nothing is left.
            let bundle = &arg[1..];
            for (at, c) in bundle.char_indices() {
                let flag =
                    FLAGS.iter().find(|f| f.short.contains(&c)).ok_or_else(|| {
                        match bundle.chars().count() {
                            1 => format!("unknown option: -{c}"),
                            _ => format!("unknown option -{c} in {arg}"),
                        }
                    })?;
                let shown = format!("-{c}");
                let rest = &bundle[at + c.len_utf8()..];
                let value = match flag.value {
                    Some(_) if !rest.is_empty() => Some(rest.to_string()),
                    Some(_) => Some(args.next().ok_or_else(|| needs_value(flag, &shown))?),
                    None => None,
                };
                let took_the_rest = value.is_some();
                let value = check(flag, &shown, value)?;
                if let Some(stop) = apply(flag.action, value, &shown, parsed)? {
                    return Ok(Some(stop));
                }
                if took_the_rest {
                    break;
                }
            }
        }
    }
    Ok(None)
}

/// `~/x` in a config file is `x` in the home directory, as in ncdu 2. Nothing else is
/// expanded (no `~user`, no variables).
fn expand_home(file: &str, home: &str) -> String {
    match file.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
        None if file == "~" => home.to_string(),
        None => file.to_string(),
    }
}

/// A file of default options.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFile {
    pub path: PathBuf,
    /// One of ncdu's own files. It was written for another program, which may have options
    /// icdu does not: what cannot be used is reported and passed over. In icdu's own files
    /// a mistake is an error.
    pub foreign: bool,
}

/// The config files and what `~` means in them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub files: Vec<ConfigFile>,
    pub home: Option<String>,
}

impl Config {
    /// The files that are read, in this order, so that each can override the ones before
    /// it: ncdu's (`/etc/ncdu.conf`, then the user's `ncdu/config`), so that an existing
    /// ncdu setup applies, then icdu's (`/etc/icdu.conf`, the user's `icdu/config`), for
    /// what is to differ and for options only icdu has. The user's files are in
    /// `$XDG_CONFIG_HOME`, or in `~/.config` if that is not set.
    pub fn from_env(home: Option<String>, xdg_config_home: Option<String>) -> Config {
        let home = home.filter(|h| !h.is_empty());
        let user_dir = match (xdg_config_home.filter(|x| !x.is_empty()), &home) {
            (Some(xdg), _) => Some(PathBuf::from(xdg)),
            (None, Some(home)) => Some(PathBuf::from(home).join(".config")),
            (None, None) => None,
        };
        let mut files = Vec::new();
        for (system, program, foreign) in [
            ("/etc/ncdu.conf", "ncdu", true),
            ("/etc/icdu.conf", "icdu", false),
        ] {
            files.push(ConfigFile {
                path: PathBuf::from(system),
                foreign,
            });
            if let Some(dir) = &user_dir {
                files.push(ConfigFile {
                    path: dir.join(program).join("config"),
                    foreign,
                });
            }
        }
        Config { files, home }
    }
}

/// What `--ignore-config` is told when a config file stops icdu.
const SKIP_HINT: &str = "Run with --ignore-config to skip the config files.";

/// Parse the command line on top of the defaults from the config files. Also returns what
/// was passed over in ncdu's files, for the user to be told.
///
/// The command line is looked at first, on its own. `--help` and `--version` have to work
/// with a broken config file, a mistake on the command line is the one to report, and
/// `--ignore-config` has to be known before anything is read.
pub fn parse_with_config(
    args: Vec<String>,
    config: &Config,
) -> Result<(Parsed, Vec<String>), String> {
    let alone = parse(args.iter().cloned())?;
    let options = args.iter().take_while(|arg| *arg != "--");
    let ignore = options.clone().any(|arg| arg == "--ignore-config");
    if ignore || !matches!(alone, Parsed::Run(_)) {
        return Ok((alone, Vec::new()));
    }
    let mut parsed = Args::default();
    let mut warnings = Vec::new();
    for file in &config.files {
        let text = match std::fs::read_to_string(&file.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warnings.push(format!("{}: {e}", file.path.display()));
                continue;
            }
        };
        for (number, problem) in read_config(&mut parsed, &text, config.home.as_deref()) {
            let problem = format!("{}:{number}: {problem}", file.path.display());
            if file.foreign {
                warnings.push(format!("{problem} (line ignored)"));
            } else {
                return Err(format!("{problem}\n{SKIP_HINT}"));
            }
        }
    }
    let mut path = None;
    match read_args(&mut parsed, &mut path, args.into_iter(), &From::CommandLine)? {
        Some(stop) => Ok((stop, warnings)),
        None => Ok((finish(parsed, path), warnings)),
    }
}

/// Apply the lines of a config file to `parsed`, and return what was wrong with which line
/// (counted from 1).
///
/// The format is ncdu's: one option per line, as it would be written on a command line,
/// with the option's value after it on the same line. The value is the rest of the line as
/// it stands, without quoting: `--exclude My Documents` excludes `My Documents`. Empty
/// lines and lines that start with `#` are passed over. A line that starts with `@` may
/// fail without that being a problem (ncdu 2), for a file that may or may not be there:
/// `@--exclude-from ~/.ncduexcludes`.
fn read_config(parsed: &mut Args, text: &str, home: Option<&str>) -> Vec<(usize, String)> {
    let mut problems = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (line, may_fail) = match line.strip_prefix('@') {
            Some(rest) => (rest.trim_start(), true),
            None => (line, false),
        };
        // The option, and after the first run of blanks its value.
        let words: Vec<String> = match line.split_once(char::is_whitespace) {
            Some((option, value)) => vec![option.to_string(), value.trim_start().to_string()],
            None => vec![line.to_string()],
        };
        let mut path = None;
        let from = From::Config { home };
        if let Err(problem) = read_args(parsed, &mut path, words.into_iter(), &from) {
            if !may_fail {
                problems.push((index + 1, problem));
            }
        }
    }
    problems
}

fn needs_value(flag: &Flag, shown: &str) -> String {
    let what = match flag.value {
        Some("N") => "a number",
        Some("FILE") => "a file (or -)",
        Some("PATTERN") => "a pattern",
        Some("DUR") => "a duration (e.g. 30d)",
        _ => "a value",
    };
    format!("{shown} needs {what}")
}

/// Carry out one flag. Returns `Some` for the flags that end parsing (`--help`, `--version`).
fn apply(
    action: Action,
    value: Option<String>,
    shown: &str,
    args: &mut Args,
) -> Result<Option<Parsed>, String> {
    let value = value.unwrap_or_default();
    match action {
        Action::Help => return Ok(Some(Parsed::Help)),
        Action::Version => return Ok(Some(Parsed::Version)),
        Action::Threads => {
            args.threads = value
                .parse::<usize>()
                .map_err(|_| format!("{shown} must be a number, not '{value}'"))?
                .max(1);
        }
        Action::Apparent(apparent) => args.disk_usage = !apparent,
        Action::OneFileSystem(one) => args.one_file_system = one,
        Action::Si(si) => args.si = si,
        Action::Color => {
            args.color = Some(match value.as_str() {
                "off" => false,
                // ncdu's two colour schemes. icdu has one.
                "dark" | "dark-bg" => true,
                _ => return Err(format!("unknown --color: {value} (off, dark or dark-bg)")),
            })
        }
        Action::Unsupported(why) => return Err(format!("{shown} is not supported by icdu: {why}")),
        Action::Exclude => args.excludes.push(value),
        Action::ExcludeFrom => {
            let content = std::fs::read_to_string(&value).map_err(|e| format!("{value}: {e}"))?;
            // One pattern per line, taken as it stands, as ncdu does: a line is not trimmed,
            // and one that starts with `#` is a pattern like any other. Only the line ending
            // is removed, and empty lines are skipped.
            args.excludes.extend(
                content
                    .split('\n')
                    .map(|line| line.trim_end_matches('\r'))
                    .filter(|line| !line.is_empty())
                    .map(str::to_string),
            );
        }
        Action::OlderThan => args.older_than = Some(parse_duration(&value)?),
        Action::NewerThan => args.newer_than = Some(parse_duration(&value)?),
        // Once: no deleting. Twice: nothing that writes to the filesystem or runs something.
        Action::ReadOnly => {
            args.read_only = args.read_only.saturating_add(1);
            args.can_delete = false;
            if args.read_only >= 2 {
                args.can_shell = false;
                args.can_export = false;
            }
        }
        // Acted on before any config file is read; see `parse_with_config`.
        Action::IgnoreConfig => {}
        Action::Delete(on) => args.can_delete = on,
        Action::Shell(on) => args.can_shell = on,
        Action::Refresh(on) => args.can_refresh = on,
        // An empty command is none: that is how a config file's setting is taken back.
        Action::DeleteCommand => args.delete_command = Some(value).filter(|c| !c.is_empty()),
        Action::ConfirmQuit(ask) => args.confirm_quit = ask,
        Action::ConfirmDelete(ask) => args.confirm_delete = ask,
        Action::ItemCount(show) => args.show_item_count = show,
        Action::Output => args.export = Some(value),
        Action::Input => args.import = Some(value),
        Action::Feedback(level) => args.feedback = Some(level),
        Action::SlowUi(slow) => args.slow_ui = slow,
        Action::Extended(on) => args.extended = on,
        Action::Mtime(show) => args.show_mtime = show,
        Action::Caches(exclude) => args.exclude_caches = exclude,
        Action::Kernfs(exclude) => args.exclude_kernfs = exclude,
        Action::FollowSymlinks(follow) => args.follow_symlinks = follow,
        Action::Hidden(show) => args.show_hidden = show,
        Action::Graph(show) => args.show_graph = show,
        Action::Percent(show) => args.show_percent = show,
        Action::GraphStyle => {
            args.graph_style = match value.as_str() {
                "hash" => GraphStyle::Hash,
                "half-block" => GraphStyle::HalfBlock,
                "eighth-block" => GraphStyle::EighthBlock,
                _ => {
                    return Err(format!(
                        "unknown --graph-style: {value} (hash, half-block or eighth-block)"
                    ))
                }
            }
        }
        // ncdu 2's column for sizes shared through hard links does not exist here yet. The
        // flag is taken, so that a command line or a config file written for ncdu runs.
        Action::SharedColumn => {
            if !matches!(value.as_str(), "off" | "shared" | "unique") {
                return Err(format!(
                    "unknown --shared-column: {value} (off, shared or unique)"
                ));
            }
        }
        Action::Sort => args.sort = Some(parse_sort(&value)?),
        Action::DirsFirst(first) => args.dirs_first = first,
        Action::Natsort(natural) => args.natsort = natural,
    }
    Ok(None)
}

/// Parse a human duration like `45s`, `30m`, `12h`, `30d`, `2w`, `1y`. A bare number is days.
pub fn parse_duration(s: &str) -> Result<u64, String> {
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

/// The OPTIONS block of `--help`, straight from [`FLAGS`]: one entry per advertised flag. A
/// flag and its negation share an entry, which stands at the place of the one with the text.
pub fn options_help() -> String {
    const SPEC_WIDTH: usize = 24;
    let mut out = String::new();
    for flag in FLAGS.iter().filter(|f| !f.help.is_empty()) {
        // "-x, --long", "    --long" or "-x": whichever spellings the flag has.
        let mut spec = match (flag.short.first(), flag.long.first()) {
            (Some(c), Some(long)) => format!("-{c}, --{long}"),
            (None, Some(long)) => format!("    --{long}"),
            (Some(c), None) => format!("-{c}"),
            (None, None) => continue,
        };
        if let Some(value) = flag.value {
            spec.push(' ');
            spec.push_str(value);
        }
        if let Some(negation) = flag.negation {
            spec.push_str(&format!(", --{negation}"));
        }
        if spec.len() > SPEC_WIDTH {
            // Too long to share a line with its text.
            out.push_str(&format!(
                "    {spec}\n    {:SPEC_WIDTH$} {}\n",
                "", flag.help
            ));
        } else {
            out.push_str(&format!("    {spec:<SPEC_WIDTH$} {}\n", flag.help));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn run(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(|a| a.to_string()))
    }

    /// The settings a command line gives, which must be one that runs.
    fn args(line: &[&str]) -> Args {
        match run(line) {
            Ok(Parsed::Run(args)) => *args,
            other => panic!("{line:?} gave {other:?}"),
        }
    }

    fn error(line: &[&str]) -> String {
        match run(line) {
            Err(e) => e,
            other => panic!("{line:?} gave {other:?}"),
        }
    }

    /// Argument lists and what they amount to, as differences from the defaults.
    #[test]
    fn command_lines_and_their_meaning() {
        let d = Args::default;
        let path = |p: &str| PathBuf::from(p);
        let table: Vec<(&[&str], Args)> = vec![
            (&[], d()),
            (
                &["/var"],
                Args {
                    path: path("/var"),
                    ..d()
                },
            ),
            (
                &["-x"],
                Args {
                    one_file_system: true,
                    ..d()
                },
            ),
            (
                &["--one-file-system"],
                Args {
                    one_file_system: true,
                    ..d()
                },
            ),
            (
                &["-a"],
                Args {
                    disk_usage: false,
                    ..d()
                },
            ),
            (
                &["--apparent"],
                Args {
                    disk_usage: false,
                    ..d()
                },
            ),
            (
                &["--apparent-size"],
                Args {
                    disk_usage: false,
                    ..d()
                },
            ),
            // A value: as the next argument, attached, or after `=`.
            (&["-t", "8"], Args { threads: 8, ..d() }),
            (&["-t8"], Args { threads: 8, ..d() }),
            (&["--threads", "8"], Args { threads: 8, ..d() }),
            (&["--threads=8"], Args { threads: 8, ..d() }),
            (&["-t", "0"], Args { threads: 1, ..d() }),
            (
                &["-o", "out.json"],
                Args {
                    export: Some("out.json".into()),
                    ..d()
                },
            ),
            (
                &["-oout.json"],
                Args {
                    export: Some("out.json".into()),
                    ..d()
                },
            ),
            (
                &["-o-"],
                Args {
                    export: Some("-".into()),
                    ..d()
                },
            ),
            (
                &["-o", "-"],
                Args {
                    export: Some("-".into()),
                    ..d()
                },
            ),
            (
                &["--output=-"],
                Args {
                    export: Some("-".into()),
                    ..d()
                },
            ),
            (
                &["-f-"],
                Args {
                    import: Some("-".into()),
                    ..d()
                },
            ),
            (
                &["--file", "dump"],
                Args {
                    import: Some("dump".into()),
                    ..d()
                },
            ),
            (
                &["--exclude=sub"],
                Args {
                    excludes: vec!["sub".into()],
                    ..d()
                },
            ),
            (
                &["--exclude", "a", "--exclude=b=c"],
                Args {
                    excludes: vec!["a".into(), "b=c".into()],
                    ..d()
                },
            ),
            // An empty value is a value.
            (
                &["--exclude="],
                Args {
                    excludes: vec!["".into()],
                    ..d()
                },
            ),
            // Bundles: flags in any number, the last one may take a value.
            (
                &["-rx"],
                Args {
                    read_only: 1,
                    can_delete: false,
                    one_file_system: true,
                    ..d()
                },
            ),
            (
                &["-xo-", "/"],
                Args {
                    one_file_system: true,
                    export: Some("-".into()),
                    path: path("/"),
                    ..d()
                },
            ),
            (
                &["-xo", "file", "/"],
                Args {
                    one_file_system: true,
                    export: Some("file".into()),
                    path: path("/"),
                    ..d()
                },
            ),
            (
                &["-xt4"],
                Args {
                    one_file_system: true,
                    threads: 4,
                    ..d()
                },
            ),
            // After a flag with a value, the rest of the bundle is the value, not more flags.
            (
                &["-oxr"],
                Args {
                    export: Some("xr".into()),
                    ..d()
                },
            ),
            // Counted, and in any spelling.
            (
                &["-r"],
                Args {
                    read_only: 1,
                    can_delete: false,
                    ..d()
                },
            ),
            (
                &["-rr"],
                Args {
                    read_only: 2,
                    can_delete: false,
                    can_shell: false,
                    can_export: false,
                    ..d()
                },
            ),
            (
                &["-r", "--read-only"],
                Args {
                    read_only: 2,
                    can_delete: false,
                    can_shell: false,
                    can_export: false,
                    ..d()
                },
            ),
            // The later of two contradicting flags wins.
            (
                &["--show-itemcount"],
                Args {
                    show_item_count: true,
                    ..d()
                },
            ),
            (&["--show-itemcount", "--hide-itemcount"], d()),
            (
                &["--hide-itemcount", "--show-itemcount"],
                Args {
                    show_item_count: true,
                    ..d()
                },
            ),
            (&["-t2", "-t", "3"], Args { threads: 3, ..d() }),
            // `--` ends the options; a lone `-` never was one.
            (
                &["--", "-x"],
                Args {
                    path: path("-x"),
                    ..d()
                },
            ),
            (
                &["-x", "--", "--help"],
                Args {
                    one_file_system: true,
                    path: path("--help"),
                    ..d()
                },
            ),
            (&["--"], d()),
            (
                &["-"],
                Args {
                    path: path("-"),
                    ..d()
                },
            ),
            // Options may follow the path.
            (
                &["/tmp", "--show-itemcount", "-rx"],
                Args {
                    path: path("/tmp"),
                    show_item_count: true,
                    read_only: 1,
                    can_delete: false,
                    one_file_system: true,
                    ..d()
                },
            ),
            (
                &["--older-than", "30d"],
                Args {
                    older_than: Some(2_592_000),
                    ..d()
                },
            ),
            (
                &["--newer-than=1y"],
                Args {
                    newer_than: Some(31_536_000),
                    ..d()
                },
            ),
            (
                &["-q"],
                Args {
                    slow_ui: true,
                    ..d()
                },
            ),
            (
                &["--slow-ui-updates"],
                Args {
                    slow_ui: true,
                    ..d()
                },
            ),
            (&["-q", "--fast-ui-updates"], d()),
            (
                &["-0"],
                Args {
                    feedback: Some(0),
                    ..d()
                },
            ),
            (
                &["-2", "-1"],
                Args {
                    feedback: Some(1),
                    ..d()
                },
            ),
            (
                &["-e"],
                Args {
                    extended: true,
                    ..d()
                },
            ),
            (
                &["--extended"],
                Args {
                    extended: true,
                    ..d()
                },
            ),
            // The last of a pair wins, as for every flag with a negation.
            (&["-e", "--no-extended"], d()),
            (
                &["--no-extended", "-e", "--show-mtime"],
                Args {
                    extended: true,
                    show_mtime: true,
                    ..d()
                },
            ),
            (&["--show-mtime", "--hide-mtime"], d()),
            (
                &["--exclude-caches"],
                Args {
                    exclude_caches: true,
                    ..d()
                },
            ),
            (&["--exclude-caches", "--include-caches"], d()),
            (
                &["--exclude-kernfs"],
                Args {
                    exclude_kernfs: true,
                    ..d()
                },
            ),
            (&["--exclude-kernfs", "--include-kernfs"], d()),
            (
                &["-L"],
                Args {
                    follow_symlinks: true,
                    ..d()
                },
            ),
            (
                &["--follow-symlinks"],
                Args {
                    follow_symlinks: true,
                    ..d()
                },
            ),
            (&["-L", "--no-follow-symlinks"], d()),
            (
                &["--sort", "name"],
                Args {
                    sort: Some((SortColumn::Name, false)),
                    ..d()
                },
            ),
            (
                &["--sort=name-desc"],
                Args {
                    sort: Some((SortColumn::Name, true)),
                    ..d()
                },
            ),
            (
                &["--sort", "disk-usage"],
                Args {
                    sort: Some((SortColumn::DiskUsage, true)),
                    ..d()
                },
            ),
            (
                &["--sort", "apparent-size-asc"],
                Args {
                    sort: Some((SortColumn::ApparentSize, false)),
                    ..d()
                },
            ),
            (
                &["--sort", "itemcount", "--sort", "mtime-asc"],
                Args {
                    sort: Some((SortColumn::Mtime, false)),
                    ..d()
                },
            ),
            (
                &["--sort=itemcount-desc"],
                Args {
                    sort: Some((SortColumn::Items, true)),
                    ..d()
                },
            ),
            (
                &["--group-directories-first"],
                Args {
                    dirs_first: true,
                    ..d()
                },
            ),
            (
                &["--group-directories-first", "--no-group-directories-first"],
                d(),
            ),
            (
                &["--disable-natsort"],
                Args {
                    natsort: false,
                    ..d()
                },
            ),
            (&["--disable-natsort", "--enable-natsort"], d()),
            (
                &["--hide-graph"],
                Args {
                    show_graph: false,
                    ..d()
                },
            ),
            (&["--hide-graph", "--show-graph"], d()),
            (
                &["--hide-hidden"],
                Args {
                    show_hidden: false,
                    ..d()
                },
            ),
            (&["--hide-hidden", "--show-hidden"], d()),
            (&["--si"], Args { si: true, ..d() }),
            (&["--si", "--no-si"], d()),
            (
                &["--color", "off"],
                Args {
                    color: Some(false),
                    ..d()
                },
            ),
            (
                &["--color=dark"],
                Args {
                    color: Some(true),
                    ..d()
                },
            ),
            (
                &["--color=off", "--color", "dark-bg"],
                Args {
                    color: Some(true),
                    ..d()
                },
            ),
            // The counterparts switch a setting back, whichever comes last.
            (&["-a", "--disk-usage"], d()),
            (
                &["--disk-usage", "--apparent-size"],
                Args {
                    disk_usage: false,
                    ..d()
                },
            ),
            (&["-x", "--cross-file-system"], d()),
            // The switches one by one, and set back.
            (
                &["--disable-delete"],
                Args {
                    can_delete: false,
                    ..d()
                },
            ),
            (&["--disable-delete", "--enable-delete"], d()),
            (
                &["--disable-shell"],
                Args {
                    can_shell: false,
                    ..d()
                },
            ),
            (&["--disable-shell", "--enable-shell"], d()),
            (
                &["--disable-refresh"],
                Args {
                    can_refresh: false,
                    ..d()
                },
            ),
            (&["--disable-refresh", "--enable-refresh"], d()),
            // A later switch wins over `-r`, and `-r` over an earlier switch.
            (
                &["-r", "--enable-delete"],
                Args {
                    read_only: 1,
                    ..d()
                },
            ),
            (
                &["--enable-delete", "-r"],
                Args {
                    read_only: 1,
                    can_delete: false,
                    ..d()
                },
            ),
            (
                &["-rr", "--enable-shell"],
                Args {
                    read_only: 2,
                    can_delete: false,
                    can_export: false,
                    ..d()
                },
            ),
            (
                &["--confirm-quit"],
                Args {
                    confirm_quit: true,
                    ..d()
                },
            ),
            (&["--confirm-quit", "--no-confirm-quit"], d()),
            (
                &["--no-confirm-delete"],
                Args {
                    confirm_delete: false,
                    ..d()
                },
            ),
            (&["--no-confirm-delete", "--confirm-delete"], d()),
            (
                &["--delete-command", "gio trash --"],
                Args {
                    delete_command: Some("gio trash --".into()),
                    ..d()
                },
            ),
            (
                &["--delete-command=rm -ri --"],
                Args {
                    delete_command: Some("rm -ri --".into()),
                    ..d()
                },
            ),
            (&["--delete-command=trash", "--delete-command="], d()),
            (
                &["--cross-file-system", "-x"],
                Args {
                    one_file_system: true,
                    ..d()
                },
            ),
            (
                &["--hide-percent", "--hide-graph"],
                Args {
                    show_graph: false,
                    show_percent: false,
                    ..d()
                },
            ),
            (&["--hide-percent", "--show-percent"], d()),
            (
                &["--graph-style", "half-block"],
                Args {
                    graph_style: GraphStyle::HalfBlock,
                    ..d()
                },
            ),
            (
                &["--graph-style=eighth-block"],
                Args {
                    graph_style: GraphStyle::EighthBlock,
                    ..d()
                },
            ),
            (
                &["--graph-style=eighth-block", "--graph-style", "hash"],
                d(),
            ),
            // Taken and ignored.
            (&["--shared-column", "unique"], d()),
            (&["--shared-column=off"], d()),
            (
                &["--no-follow-symlinks", "-xL"],
                Args {
                    follow_symlinks: true,
                    one_file_system: true,
                    ..d()
                },
            ),
            (
                &["--include-caches", "--exclude-caches"],
                Args {
                    exclude_caches: true,
                    ..d()
                },
            ),
        ];
        for (line, want) in table {
            assert_eq!(args(line), want, "{line:?}");
        }
    }

    /// The command lines from the EXAMPLES sections of the ncdu 1.18 and 2.x man pages.
    #[test]
    fn examples_of_the_ncdu_man_page_parse() {
        let d = Args::default;
        assert_eq!(args(&[]), d());
        assert_eq!(
            args(&["-x", "/"]),
            Args {
                one_file_system: true,
                path: "/".into(),
                ..d()
            }
        );
        assert_eq!(
            args(&["-1xo-", "/"]),
            Args {
                feedback: Some(1),
                one_file_system: true,
                export: Some("-".into()),
                path: "/".into(),
                ..d()
            }
        );
        assert_eq!(
            args(&["-f-"]),
            Args {
                import: Some("-".into()),
                ..d()
            }
        );
        assert_eq!(
            args(&["-o-"]),
            Args {
                export: Some("-".into()),
                ..d()
            }
        );
        assert_eq!(
            args(&["-o-", "/"]),
            Args {
                export: Some("-".into()),
                path: "/".into(),
                ..d()
            }
        );
        assert_eq!(
            args(&["-e", "-f-"]),
            Args {
                extended: true,
                import: Some("-".into()),
                ..d()
            }
        );
        assert_eq!(
            args(&["-0xo-", "/"]).feedback,
            Some(0),
            "the cron variant of the export example"
        );
    }

    /// Without `-0`, `-1` or `-2` the feedback follows from where the dump goes.
    #[test]
    fn feedback_mode_defaults_follow_the_output() {
        let mode = |line: &[&str]| args(line).feedback_mode();
        // Nothing to export: the live browser.
        assert_eq!(mode(&[]), Feedback::Browser);
        assert_eq!(mode(&["/tmp"]), Feedback::Browser);
        // A dump on standard output: silence. A dump in a file: one line.
        assert_eq!(mode(&["-o-"]), Feedback::Silent);
        assert_eq!(mode(&["-o", "dump.json"]), Feedback::Line);
        // Asked for explicitly.
        assert_eq!(mode(&["-0"]), Feedback::Silent);
        assert_eq!(mode(&["-1"]), Feedback::Line);
        assert_eq!(mode(&["-2"]), Feedback::Browser);
        assert_eq!(mode(&["-1", "-o-"]), Feedback::Line);
        assert_eq!(mode(&["-0", "-o", "dump.json"]), Feedback::Silent);
        // There is no browser while a dump is written.
        assert_eq!(mode(&["-2", "-o-"]), Feedback::Line);
        // The last of several wins.
        assert_eq!(mode(&["-0", "-1", "-o-"]), Feedback::Line);
        assert_eq!(
            mode(&["-1xo-", "/"]),
            Feedback::Line,
            "ncdu's man page example"
        );
    }

    #[test]
    fn help_and_version_end_parsing() {
        for line in [
            &["-h"][..],
            &["--help"],
            &["-xh"],
            &["/tmp", "--help", "--no-such"],
        ] {
            assert_eq!(run(line), Ok(Parsed::Help), "{line:?}");
        }
        for line in [&["-V"][..], &["-v"], &["--version"], &["-rV"]] {
            assert_eq!(run(line), Ok(Parsed::Version), "{line:?}");
        }
    }

    /// What is wrong with a command line is said in terms of what was typed.
    #[test]
    fn errors_name_the_argument() {
        let table: &[(&[&str], &str)] = &[
            (&["--no-such"], "unknown option: --no-such"),
            // Flags of ncdu's that icdu knows and does not have say so, and why.
            (
                &["-O", "dump.bin"],
                "-O is not supported by icdu: there is no binary export; -o FILE writes JSON",
            ),
            (
                &["-c"],
                "-c is not supported by icdu: dumps are written uncompressed; pipe one \
                 through zstd or gzip (icdu -o- DIR | zstd > FILE)",
            ),
            (
                &["--compress-level=9"],
                "--compress-level is not supported by icdu: dumps are written uncompressed; \
                 pipe one through zstd or gzip (icdu -o- DIR | zstd > FILE)",
            ),
            (
                &["--export-block-size", "64"],
                "--export-block-size is not supported by icdu: it belongs to the binary \
                 export, which there is none of",
            ),
            (
                &["--color", "blue"],
                "unknown --color: blue (off, dark or dark-bg)",
            ),
            (
                &["--graph-style", "dots"],
                "unknown --graph-style: dots (hash, half-block or eighth-block)",
            ),
            (&["--no-such=1"], "unknown option: --no-such"),
            (&["-Z"], "unknown option: -Z"),
            (&["-xZ"], "unknown option -Z in -xZ"),
            (&["-t"], "-t needs a number"),
            (&["--threads"], "--threads needs a number"),
            (&["-xo"], "-o needs a file (or -)"),
            (&["--exclude"], "--exclude needs a pattern"),
            (
                &["--older-than"],
                "--older-than needs a duration (e.g. 30d)",
            ),
            (&["-t", "many"], "-t must be a number, not 'many'"),
            (&["--threads=x"], "--threads must be a number, not 'x'"),
            (&["--read-only=yes"], "--read-only does not take a value"),
            (&["--help=me"], "--help does not take a value"),
            (&["a", "b"], "more than one directory given: a and b"),
            (
                &["--older-than", "soon"],
                "invalid duration 'soon' (expected e.g. 45s, 30m, 12h, 30d, 2w, 1y)",
            ),
            (
                &["-X", "/nonexistent/icdu/excludes"],
                "/nonexistent/icdu/excludes: No such file or directory (os error 2)",
            ),
        ];
        for (line, want) in table {
            assert_eq!(error(line), *want, "{line:?}");
        }
    }

    /// `-X` reads patterns from a file, one per line, exactly as ncdu 1.18 does (checked
    /// with files of each kind): a line is a pattern as it stands, with its spaces, and also
    /// when it starts with `#`. Line endings are removed and empty lines skipped.
    #[test]
    fn exclude_from_reads_patterns() {
        let file = std::env::temp_dir().join(format!("icdu_args_{}", std::process::id()));
        std::fs::write(
            &file,
            "*.o\n\n#hash\n lead\ntrail \n  padded  \r\nlast-without-newline",
        )
        .unwrap();
        let file_arg = file.to_string_lossy().to_string();
        let parsed = args(&["--exclude", "first", "-X", &file_arg]);
        assert_eq!(
            parsed.excludes,
            vec![
                "first",
                "*.o",
                "#hash",
                " lead",
                "trail ",
                "  padded  ",
                "last-without-newline"
            ]
        );
        assert_eq!(args(&[&format!("-X{file_arg}")]).excludes.len(), 6);
        assert_eq!(
            args(&[&format!("--exclude-from={file_arg}")])
                .excludes
                .len(),
            6
        );
        std::fs::remove_file(&file).unwrap();
    }

    /// The defaults, to spell a result as its differences from them.
    fn d() -> Args {
        Args::default()
    }

    /// A directory for config files of one test.
    fn config_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("icdu_config_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file(dir: &Path, name: &str, text: &str, foreign: bool) -> ConfigFile {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        ConfigFile { path, foreign }
    }

    fn with_config(args: &[&str], config: &Config) -> Result<(Args, Vec<String>), String> {
        let args = args.iter().map(|a| a.to_string()).collect();
        match parse_with_config(args, config)? {
            (Parsed::Run(parsed), warnings) => Ok((*parsed, warnings)),
            (other, _) => panic!("{other:?}"),
        }
    }

    /// Which files are read, and in which order: ncdu's before icdu's, the system's before
    /// the user's, and the user's in `$XDG_CONFIG_HOME` if that is set to something.
    #[test]
    fn config_files_are_ncdus_and_then_icdus() {
        let paths = |home: Option<&str>, xdg: Option<&str>| -> Vec<(String, bool)> {
            Config::from_env(home.map(String::from), xdg.map(String::from))
                .files
                .into_iter()
                .map(|f| (f.path.to_string_lossy().to_string(), f.foreign))
                .collect()
        };
        let named = |list: &[(&str, bool)]| -> Vec<(String, bool)> {
            list.iter().map(|&(p, f)| (p.to_string(), f)).collect()
        };
        assert_eq!(
            paths(Some("/home/me"), None),
            named(&[
                ("/etc/ncdu.conf", true),
                ("/home/me/.config/ncdu/config", true),
                ("/etc/icdu.conf", false),
                ("/home/me/.config/icdu/config", false),
            ])
        );
        assert_eq!(
            paths(Some("/home/me"), Some("/xdg")),
            named(&[
                ("/etc/ncdu.conf", true),
                ("/xdg/ncdu/config", true),
                ("/etc/icdu.conf", false),
                ("/xdg/icdu/config", false),
            ])
        );
        // An empty variable is no variable.
        assert_eq!(
            paths(Some("/home/me"), Some("")),
            paths(Some("/home/me"), None)
        );
        // Without a home there are only the system's files.
        assert_eq!(
            paths(None, None),
            named(&[("/etc/ncdu.conf", true), ("/etc/icdu.conf", false)])
        );
        assert_eq!(paths(Some(""), None), paths(None, None));
        assert_eq!(
            Config::from_env(Some("/home/me".into()), None)
                .home
                .as_deref(),
            Some("/home/me")
        );
    }

    /// The acceptance case: a config written for ncdu takes effect.
    #[test]
    fn an_ncdu_config_takes_effect() {
        let dir = config_dir("ncdu");
        let config = Config {
            files: vec![file(
                &dir,
                "config",
                "# Always enable extended mode\n-e\n\n# Disable file deletion\n--disable-delete\n\n\
                 # Exclude .git directories\n--exclude .git\n",
                true,
            )],
            home: None,
        };
        let (args, warnings) = with_config(&["/data"], &config).unwrap();
        assert_eq!(
            args,
            Args {
                extended: true,
                can_delete: false,
                excludes: vec![".git".into()],
                path: "/data".into(),
                ..d()
            }
        );
        assert!(warnings.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The format of a line, as ncdu 1.18 reads it (probed with files like these), plus the
    /// two things ncdu 2 adds: `--option=value` and the `@` prefix.
    #[test]
    fn config_lines_are_read_like_ncdu_reads_them() {
        let read = |text: &str| {
            let mut parsed = Args::default();
            let problems = read_config(&mut parsed, text, Some("/home/me"));
            (parsed, problems)
        };
        let excludes = |text: &str| {
            let (parsed, problems) = read(text);
            assert!(problems.is_empty(), "{text:?}: {problems:?}");
            parsed.excludes
        };
        // The value is the rest of the line: no quotes, no comment after it, blanks inside
        // it kept, blanks around it dropped.
        assert_eq!(excludes("--exclude sub\n"), ["sub"]);
        assert_eq!(excludes("--exclude   sub  \n"), ["sub"]);
        assert_eq!(excludes("  --exclude sub\n"), ["sub"]);
        assert_eq!(excludes("\t--exclude\tsub\n"), ["sub"]);
        assert_eq!(excludes("--exclude pad dir\n"), ["pad dir"]);
        assert_eq!(excludes("--exclude \"sub\"\n"), ["\"sub\""]);
        assert_eq!(excludes("--exclude sub # trailing\n"), ["sub # trailing"]);
        assert_eq!(
            excludes("--exclude ~/x\n"),
            ["~/x"],
            "only file names get a home"
        );
        assert_eq!(excludes("--exclude=sub\n"), ["sub"]);
        // Comments, also indented, empty lines, a last line without its end, and CR LF.
        assert_eq!(
            excludes("# one\n   # two\n\n\n--exclude a\r\n--exclude b"),
            ["a", "b"]
        );
        // Short flags can be bundled, as on a command line.
        let (parsed, problems) = read("-ex\n");
        assert!(problems.is_empty());
        assert!(parsed.extended && parsed.one_file_system);

        // What is wrong is reported with its line, and the lines around it still count.
        let (parsed, problems) = read(
            "-e\n--no-such-flag\n--exclude\n-e --exclude c\nsub\n--sort sideways\n--exclude kept\n",
        );
        assert!(parsed.extended);
        assert_eq!(parsed.excludes, ["kept"]);
        assert_eq!(
            problems,
            [
                (2, "unknown option: --no-such-flag".to_string()),
                (3, "--exclude needs a pattern".to_string()),
                (4, "unknown option: --exclude c".to_string()),
                (5, "not an option: sub".to_string()),
                (
                    6,
                    "unknown --sort column: sideways (disk-usage, name, apparent-size, \
                     itemcount or mtime, optionally followed by -asc or -desc)"
                        .to_string()
                ),
            ]
        );
        // What belongs to one run cannot be a default.
        for line in [
            "--help",
            "-h",
            "--version",
            "-o out.json",
            "-f in.json",
            "--ignore-config",
        ] {
            let (_, problems) = read(line);
            assert_eq!(problems.len(), 1, "{line}");
            assert!(
                problems[0].1.ends_with("cannot be set in a config file"),
                "{problems:?}"
            );
        }
        // `@`: a line that may fail.
        let (parsed, problems) = read("@--no-such-flag\n@--exclude-from /no/such/file\n@ -e\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert!(parsed.extended, "and one that works does");
    }

    /// `~/` in the file name of `-X` is the home directory, in a config file only.
    #[test]
    fn exclude_files_in_a_config_are_found_in_the_home_directory() {
        let home = config_dir("home");
        std::fs::write(home.join("patterns"), "*.o\nbuild\n").unwrap();
        let home_str = home.to_string_lossy().to_string();
        for line in ["-X ~/patterns", "--exclude-from ~/patterns", "-X~/patterns"] {
            let mut parsed = Args::default();
            let problems = read_config(&mut parsed, line, Some(&home_str));
            assert!(problems.is_empty(), "{line}: {problems:?}");
            assert_eq!(parsed.excludes, ["*.o", "build"], "{line}");
        }
        assert_eq!(expand_home("~/a/b", "/home/me"), "/home/me/a/b");
        assert_eq!(expand_home("~/a", "/home/me/"), "/home/me/a");
        assert_eq!(expand_home("~", "/home/me"), "/home/me");
        assert_eq!(expand_home("~other/a", "/home/me"), "~other/a");
        assert_eq!(expand_home("/abs/~/a", "/home/me"), "/abs/~/a");
        // On a command line the shell has done that, and icdu does not do it again.
        let err = parse(["-X".to_string(), "~/patterns".to_string()]).unwrap_err();
        assert!(err.starts_with("~/patterns: "), "{err}");
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// Later files override earlier ones, and the command line overrides them all, also
    /// with the counterpart of a flag. What only adds (excludes) adds up.
    #[test]
    fn the_command_line_has_the_last_word() {
        let dir = config_dir("order");
        let config = Config {
            files: vec![
                file(
                    &dir,
                    "ncdu.conf",
                    "-e\n--exclude system\n--sort name\n",
                    true,
                ),
                file(
                    &dir,
                    "ncdu-user",
                    "--no-extended\n-x\n--exclude user\n--si\n",
                    true,
                ),
                file(
                    &dir,
                    "icdu-user",
                    "--sort itemcount-asc\n--older-than 30d\n",
                    false,
                ),
            ],
            home: None,
        };
        let (args, warnings) = with_config(&[], &config).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            args,
            Args {
                one_file_system: true,
                si: true,
                excludes: vec!["system".into(), "user".into()],
                sort: Some((SortColumn::Items, false)),
                older_than: Some(30 * 86_400),
                ..d()
            }
        );
        let (args, _) = with_config(
            &[
                "--cross-file-system",
                "--no-si",
                "-e",
                "--exclude",
                "mine",
                "--sort=mtime",
            ],
            &config,
        )
        .unwrap();
        assert_eq!(
            args,
            Args {
                extended: true,
                excludes: vec!["system".into(), "user".into(), "mine".into()],
                sort: Some((SortColumn::Mtime, true)),
                older_than: Some(30 * 86_400),
                ..d()
            }
        );
        // `--ignore-config`: as if there were no files. Wherever it stands, but not as the
        // name of a directory after `--`.
        let (args, _) = with_config(&["-x", "--ignore-config"], &config).unwrap();
        assert_eq!(
            args,
            Args {
                one_file_system: true,
                ..d()
            }
        );
        let (args, _) = with_config(&["--", "--ignore-config"], &config).unwrap();
        assert!(args.si, "the files were read");
        assert_eq!(args.path, PathBuf::from("--ignore-config"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A mistake in one of ncdu's files is reported and passed over: the file was written
    /// for another program. In one of icdu's own it stops icdu, with file and line. A file
    /// that is not there is nothing to speak of.
    #[test]
    fn config_mistakes_warn_in_ncdus_files_and_stop_in_icdus() {
        let dir = config_dir("mistakes");
        let ncdu = file(
            &dir,
            "ncdu",
            "-e\n-O dump.bin\n--some-new-ncdu-option\n-x\n",
            true,
        );
        let missing = ConfigFile {
            path: dir.join("not-there"),
            foreign: false,
        };
        let config = Config {
            files: vec![ncdu.clone(), missing.clone()],
            home: None,
        };
        let (args, warnings) = with_config(&[], &config).unwrap();
        assert!(
            args.extended && args.one_file_system,
            "the other lines count"
        );
        let at = |line: usize| format!("{}:{line}: ", ncdu.path.display());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with(&(at(2) + "-O is not supported by icdu")));
        assert_eq!(
            warnings[1],
            at(3) + "unknown option: --some-new-ncdu-option (line ignored)"
        );

        let own = file(&dir, "icdu", "--si\n--colour off\n", false);
        let config = Config {
            files: vec![ncdu, missing, own.clone()],
            home: None,
        };
        let err = with_config(&[], &config).unwrap_err();
        assert_eq!(
            err,
            format!(
                "{}:2: unknown option: --colour\nRun with --ignore-config to skip the config files.",
                own.path.display()
            )
        );
        // With a broken file, `--help`, `--version` and `--ignore-config` still work, and a
        // mistake on the command line is the one that is reported.
        let run = |args: &[&str]| {
            parse_with_config(args.iter().map(|a| a.to_string()).collect(), &config)
        };
        assert_eq!(run(&["--help"]).unwrap().0, Parsed::Help);
        assert_eq!(run(&["-V"]).unwrap().0, Parsed::Version);
        assert!(matches!(
            run(&["--ignore-config"]).unwrap().0,
            Parsed::Run(_)
        ));
        assert_eq!(
            run(&["--nonsense"]).unwrap_err(),
            "unknown option: --nonsense"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The manual page documents every option the parser knows, under the name `--help`
    /// uses, and no option the parser does not know. A flag added to the table and not to
    /// the page fails here.
    #[test]
    fn man_page_lists_the_same_options() {
        // The page writes a dash as `\-`.
        let page = include_str!("../doc/icdu.1").replace("\\-", "-");
        // The words of the page that are options: after a blank, a quote or a bracket.
        let words: Vec<&str> = page
            .split(|c: char| c.is_whitespace() || "\"',()[]\\".contains(c))
            .map(|word| word.trim_end_matches(['.', ',', ':', ';']))
            .collect();
        for flag in FLAGS {
            let long = flag.long.first().map(|name| format!("--{name}"));
            let short = flag.short.first().map(|c| format!("-{c}"));
            for spelling in long.iter().chain(short.iter()) {
                // `--sort=name-desc` and `-o-` count as a mention of `--sort` and `-o`.
                let named = words.iter().any(|word| {
                    word == spelling
                        || word
                            .strip_prefix(spelling.as_str())
                            .is_some_and(|rest| rest.starts_with('=') || rest == "-")
                });
                assert!(named, "doc/icdu.1 does not mention {spelling}");
            }
        }
        for word in &words {
            let Some(name) = word.strip_prefix("--") else {
                continue;
            };
            let name = name.split('=').next().unwrap_or(name);
            if name.is_empty() {
                continue; // `--` on its own ends the options
            }
            assert!(
                FLAGS.iter().any(|f| f.long.contains(&name)),
                "doc/icdu.1 names --{name}, which is not an option"
            );
        }
    }

    /// The table is consistent: no spelling is used twice, every negation exists and points
    /// back, and a flag that is advertised has a long name to show.
    #[test]
    fn flag_table_is_consistent() {
        let mut shorts: Vec<char> = FLAGS.iter().flat_map(|f| f.short.iter().copied()).collect();
        let mut longs: Vec<&str> = FLAGS.iter().flat_map(|f| f.long.iter().copied()).collect();
        let (short_count, long_count) = (shorts.len(), longs.len());
        shorts.sort_unstable();
        shorts.dedup();
        longs.sort_unstable();
        longs.dedup();
        assert_eq!(shorts.len(), short_count, "a short flag is listed twice");
        assert_eq!(longs.len(), long_count, "a long flag is listed twice");

        for flag in FLAGS {
            assert!(!flag.short.is_empty() || !flag.long.is_empty());
            if let Some(negation) = flag.negation {
                let other = FLAGS
                    .iter()
                    .find(|f| f.long.contains(&negation))
                    .unwrap_or_else(|| panic!("--{negation} is not in the table"));
                assert!(
                    other.negation.is_some_and(|back| flag.long.contains(&back)),
                    "--{negation} does not point back"
                );
            }
        }
    }

    /// `--help` lists every advertised flag with its spellings and its value, a negation
    /// next to the flag it undoes, and nothing the parser does not accept.
    #[test]
    fn help_lists_what_the_parser_accepts() {
        let help = options_help();
        let mentions = |long: &str| {
            help.split(|c: char| c.is_whitespace() || c == ',')
                .any(|word| word == long)
        };
        for flag in FLAGS.iter().filter(|f| !f.help.is_empty()) {
            assert!(
                help.contains(flag.help),
                "{}: its text is missing",
                flag.help
            );
            let Some(long) = flag.long.first().map(|long| format!("--{long}")) else {
                // Short only: listed by its letter.
                let short = format!("-{}", flag.short[0]);
                assert!(
                    help.lines().any(|l| l.trim_start().starts_with(&short)),
                    "{short} is missing from --help"
                );
                continue;
            };
            assert!(mentions(&long), "{long} is missing from --help");
            if let Some(c) = flag.short.first() {
                assert!(help.contains(&format!("-{c}, {long}")), "{long}");
            }
            if let Some(value) = flag.value {
                assert!(help.contains(&format!("{long} {value}")), "{long}");
            }
            if let Some(negation) = flag.negation {
                assert!(
                    help.contains(&format!("{long}, --{negation}")),
                    "{long} without its negation"
                );
            }
        }
        // Every option the help names is one the parser knows.
        for word in help.split(|c: char| c.is_whitespace() || c == ',') {
            if let Some(name) = word.strip_prefix("--") {
                assert!(
                    FLAGS.iter().any(|f| f.long.contains(&name)),
                    "--help names --{name}, which is not a flag"
                );
            }
        }
        // No line runs past the usual 80 columns.
        for line in help.lines() {
            assert!(line.chars().count() <= 80, "{line}");
        }
    }

    /// The option list in the README is the one `--help` prints.
    #[test]
    fn readme_lists_the_same_options() {
        let readme = include_str!("../README.md");
        for line in options_help().lines() {
            assert!(readme.contains(line), "README lacks: {line}");
        }
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
