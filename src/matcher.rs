//! Matching a search query against entry names and paths, ignoring case.
//!
//! A query is one of three kinds ([`Query`]): text to find in a name, text with a `/` to find
//! in a path, or a pattern with `*` or `?` that a whole name (or, with a `/`, a path) has to
//! match.
//!
//! Almost all names and queries are ASCII, and for those the match is a byte comparison that
//! ignores ASCII case. A query with other letters takes a slower path that folds case with
//! `char::to_lowercase`, so `übung` finds `Übung.pdf`, and that also tries the query in its
//! other Unicode spelling: `ü` can be one code point (composed) or `u` followed by a combining
//! diaeresis (decomposed, which is how macOS file systems commonly store it).

use std::borrow::Cow;
use std::ops::Range;

use crate::glob::Pattern;

/// A search query, prepared once, to be matched against many entries.
///
/// - **A name**: the query is found anywhere in the entry's name. This is the common case and
///   costs one [`Matcher`] per name.
/// - **A path**, if the query has a `/`: it is found in the entry's path below the directory
///   searched from, written as `/a/b/name`, with a `/` after the name of a directory. The
///   entry is a hit if the match ends in its own name: `src/main` finds `…/src/main.rs` (and
///   not what is in a directory `src/main`), `src/` finds directories called `src`, and a
///   leading `/` holds the match to the top: `/src` finds `src` there and nowhere below.
/// - **A pattern**, if the query has a `*` or a `?`: the whole name has to match it, so
///   `*.iso` finds names that end in `.iso`. With a `/` it is matched against the path and
///   every tail of it that starts after a `/`, and as in a shell, `*` and `?` stay within
///   one component: `src/*.rs` finds `…/src/main.rs` and not `…/src/bin/x.rs`. A `/` at the
///   end asks for directories.
///
/// Case is ignored in all three.
pub struct Query {
    kind: Kind,
}

enum Kind {
    Name(Matcher),
    Path {
        /// The query, to be found in the path.
        whole: Matcher,
        /// The part of the query that has to be in the name (the text after its last `/`, or
        /// before it if it ends with one). Names without it need no path to be built.
        part: Matcher,
        /// The query ends with a `/`: only directories can match.
        dirs: bool,
        /// The query starts with a `/`: the match has to start at the top.
        top: bool,
    },
    Glob {
        /// The pattern, in lower case.
        pattern: Pattern,
        /// It has a `/`: it is matched against paths.
        path: bool,
        dirs: bool,
    },
}

impl Query {
    pub fn new(query: &str) -> Self {
        let kind = if query.contains(['*', '?']) {
            let (text, dirs) = match query.strip_suffix('/') {
                Some(text) if !text.is_empty() => (text, true),
                _ => (query, false),
            };
            Kind::Glob {
                pattern: Pattern::within_components(&text.to_lowercase()),
                path: text.contains('/'),
                dirs,
            }
        } else if query.contains('/') {
            let dirs = query.ends_with('/');
            let trimmed = query.trim_end_matches('/');
            let part = if dirs {
                trimmed.rsplit('/').next().unwrap_or("")
            } else {
                query.rsplit('/').next().unwrap_or("")
            };
            Kind::Path {
                whole: Matcher::new(query),
                part: Matcher::new(part),
                dirs,
                // `/` alone is every directory: the `/` after its name.
                top: query.starts_with('/') && !trimmed.is_empty(),
            }
        } else {
            Kind::Name(Matcher::new(query))
        };
        Query { kind }
    }

    /// The matcher of a query for names, the common kind. A search over millions of
    /// entries uses it directly: it is the fastest way through.
    pub fn name_matcher(&self) -> Option<&Matcher> {
        match &self.kind {
            Kind::Name(matcher) => Some(matcher),
            _ => None,
        }
    }

    /// True if the entry's path has to be looked at as well (see [`Query::matches_path`]).
    pub fn wants_path(&self) -> bool {
        matches!(self.kind, Kind::Path { .. } | Kind::Glob { path: true, .. })
    }

    /// What can be told from the entry's name alone. For a query that [wants the
    /// path](Query::wants_path), false rules the entry out and true means that its path has
    /// to be looked at.
    pub fn matches_name(&self, name: &str, is_dir: bool) -> bool {
        match &self.kind {
            Kind::Name(matcher) => matcher.matches(name),
            Kind::Path { part, dirs, .. } => (is_dir || !dirs) && part.matches(name),
            Kind::Glob {
                pattern,
                path,
                dirs,
            } => (is_dir || !dirs) && (*path || pattern.matches(lower(name).as_bytes())),
        }
    }

    /// For an entry whose name passed [`Query::matches_name`]: true if its `path` matches.
    /// The path is the one [`Query`] describes (a leading `/`, a trailing one for a
    /// directory), and its last component, the entry's name, starts at `name_start`.
    pub fn matches_path(&self, path: &str, name_start: usize) -> bool {
        match &self.kind {
            Kind::Name(_) => true,
            Kind::Path { .. } => self.path_match(path, name_start).is_some(),
            Kind::Glob {
                pattern,
                path: true,
                ..
            } => {
                let path = lower(path.strip_suffix('/').unwrap_or(path));
                let text = path.as_bytes();
                pattern.matches(text)
                    || (0..text.len())
                        .filter(|&i| text[i] == b'/')
                        .any(|i| pattern.matches(&text[i + 1..]))
            }
            Kind::Glob { .. } => true,
        }
    }

    /// The part of `path` to highlight for an entry that matched; `path` and `name_start` as
    /// for [`Query::matches_path`]. A pattern marks the whole name.
    pub fn highlight(&self, path: &str, name_start: usize) -> Option<Range<usize>> {
        let name_end = path.len() - usize::from(path.ends_with('/') && path.len() > name_start);
        let name = &path[name_start..name_end];
        match &self.kind {
            Kind::Name(matcher) => matcher
                .find(name)
                .map(|r| r.start + name_start..r.end + name_start),
            Kind::Path { .. } => self.path_match(path, name_start),
            Kind::Glob { .. } => Some(name_start..name_end),
        }
    }

    /// Where a path query matches `path` so that the match reaches into the name.
    fn path_match(&self, path: &str, name_start: usize) -> Option<Range<usize>> {
        let Kind::Path { whole, top, .. } = &self.kind else {
            return None;
        };
        if *top {
            // Every component has a `/` in front of it: only the first is at the top.
            whole
                .find(path)
                .filter(|found| found.start == 0 && found.end > name_start)
        } else {
            whole.find_ending_after(path, name_start)
        }
    }
}

/// `text` in lower case, as patterns are matched. Most names have no capital letter and are
/// used as they are.
fn lower(text: &str) -> Cow<'_, str> {
    if text.is_ascii() && !text.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(text.to_lowercase())
    }
}

/// A query, prepared once, to be matched against many names.
pub struct Matcher {
    /// The query as typed, for the ASCII path.
    ascii: Option<Box<[u8]>>,
    /// The query in lower case, in each spelling worth trying, for everything else.
    folded: Vec<Vec<char>>,
}

impl Matcher {
    pub fn new(query: &str) -> Self {
        if query.is_ascii() {
            return Matcher {
                ascii: Some(query.as_bytes().into()),
                folded: Vec::new(),
            };
        }
        let lower: String = query.chars().flat_map(char::to_lowercase).collect();
        let mut folded = vec![lower.chars().collect::<Vec<char>>()];
        for other in [compose(&lower), decompose(&lower)] {
            let other: Vec<char> = other.chars().collect();
            if !folded.contains(&other) {
                folded.push(other);
            }
        }
        Matcher {
            ascii: None,
            folded,
        }
    }

    /// True if the query occurs in `name`.
    pub fn matches(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// The first occurrence of the query in `text` that ends after byte `min_end`.
    pub fn find_ending_after(&self, text: &str, min_end: usize) -> Option<Range<usize>> {
        let mut from = 0;
        while from < text.len() {
            let found = self.find(&text[from..])?;
            let found = found.start + from..found.end + from;
            if found.end > min_end {
                return Some(found);
            }
            // Try again one character further on.
            from = found.start + text[found.start..].chars().next().map_or(1, char::len_utf8);
        }
        None
    }

    /// The first occurrence of the query in `name`, as a byte range of `name`. It starts and
    /// ends on character boundaries. With case folding the matched text can be longer or
    /// shorter than the query, so the range is what has to be highlighted, not the query's
    /// length. An empty query matches at the start.
    pub fn find(&self, name: &str) -> Option<Range<usize>> {
        if let Some(query) = &self.ascii {
            // Bytes of a multi-byte character never equal an ASCII byte, so a byte window
            // that matches begins and ends on character boundaries.
            if query.is_empty() {
                return Some(0..0);
            }
            return name
                .as_bytes()
                .windows(query.len())
                .position(|window| window.eq_ignore_ascii_case(query))
                .map(|start| start..start + query.len());
        }
        // A query with a letter outside ASCII cannot occur in an ASCII name.
        if name.is_ascii() {
            return None;
        }
        self.folded
            .iter()
            .filter_map(|query| find_folded(name, query))
            .min_by_key(|range| range.start)
    }
}

/// The first place in `name` where its lower-cased characters spell `query`.
fn find_folded(name: &str, query: &[char]) -> Option<Range<usize>> {
    name.char_indices().find_map(|(start, _)| {
        let mut wanted = query.iter();
        let mut end = start;
        for c in name[start..].chars() {
            // One character can fold to several (`İ` becomes `i` and a combining dot). The
            // match has to cover all of them: it cannot end inside a character of the name.
            for lower in c.to_lowercase() {
                if wanted.next() != Some(&lower) {
                    return None;
                }
            }
            end += c.len_utf8();
            if wanted.len() == 0 {
                return Some(start..end);
            }
        }
        None
    })
}

/// Lower-case Latin letters that Unicode can spell in two ways: as one code point, or as a
/// base letter followed by a combining mark. Not a full normalisation table; these are the
/// letters of the European languages written in Latin script, which is where a query typed
/// on one system meets names stored by another.
const LETTERS: &[(char, char, char)] = &[
    ('à', 'a', '\u{300}'),
    ('á', 'a', '\u{301}'),
    ('â', 'a', '\u{302}'),
    ('ã', 'a', '\u{303}'),
    ('ä', 'a', '\u{308}'),
    ('å', 'a', '\u{30a}'),
    ('ă', 'a', '\u{306}'),
    ('ą', 'a', '\u{328}'),
    ('ç', 'c', '\u{327}'),
    ('ć', 'c', '\u{301}'),
    ('č', 'c', '\u{30c}'),
    ('ď', 'd', '\u{30c}'),
    ('è', 'e', '\u{300}'),
    ('é', 'e', '\u{301}'),
    ('ê', 'e', '\u{302}'),
    ('ë', 'e', '\u{308}'),
    ('ě', 'e', '\u{30c}'),
    ('ę', 'e', '\u{328}'),
    ('ğ', 'g', '\u{306}'),
    ('ì', 'i', '\u{300}'),
    ('í', 'i', '\u{301}'),
    ('î', 'i', '\u{302}'),
    ('ï', 'i', '\u{308}'),
    ('ñ', 'n', '\u{303}'),
    ('ń', 'n', '\u{301}'),
    ('ň', 'n', '\u{30c}'),
    ('ò', 'o', '\u{300}'),
    ('ó', 'o', '\u{301}'),
    ('ô', 'o', '\u{302}'),
    ('õ', 'o', '\u{303}'),
    ('ö', 'o', '\u{308}'),
    ('ő', 'o', '\u{30b}'),
    ('ř', 'r', '\u{30c}'),
    ('ś', 's', '\u{301}'),
    ('š', 's', '\u{30c}'),
    ('ş', 's', '\u{327}'),
    ('ș', 's', '\u{326}'),
    ('ť', 't', '\u{30c}'),
    ('ț', 't', '\u{326}'),
    ('ù', 'u', '\u{300}'),
    ('ú', 'u', '\u{301}'),
    ('û', 'u', '\u{302}'),
    ('ü', 'u', '\u{308}'),
    ('ů', 'u', '\u{30a}'),
    ('ű', 'u', '\u{30b}'),
    ('ý', 'y', '\u{301}'),
    ('ÿ', 'y', '\u{308}'),
    ('ź', 'z', '\u{301}'),
    ('ż', 'z', '\u{307}'),
    ('ž', 'z', '\u{30c}'),
];

/// `text` with every base letter and combining mark from [`LETTERS`] joined into one
/// character.
fn compose(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let joined = chars.peek().and_then(|&mark| {
            LETTERS
                .iter()
                .find(|&&(_, base, m)| base == c && m == mark)
                .map(|&(composed, _, _)| composed)
        });
        match joined {
            Some(composed) => {
                out.push(composed);
                chars.next();
            }
            None => out.push(c),
        }
    }
    out
}

/// `text` with every character from [`LETTERS`] split into its base letter and mark.
fn decompose(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        match LETTERS.iter().find(|&&(composed, _, _)| composed == c) {
            Some(&(_, base, mark)) => out.extend([base, mark]),
            None => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `query` finds the entry with the search path `path` (see [`Query`]), and what
    /// it highlights there.
    fn query_hit<'a>(query: &str, path: &'a str) -> Option<&'a str> {
        let query = Query::new(query);
        let is_dir = path.ends_with('/');
        let name_start = path.trim_end_matches('/').rfind('/').unwrap() + 1;
        let name = path[name_start..].trim_end_matches('/');
        if !query.matches_name(name, is_dir) {
            return None;
        }
        if query.wants_path() && !query.matches_path(path, name_start) {
            return None;
        }
        query.highlight(path, name_start).map(|range| &path[range])
    }

    #[test]
    fn a_query_with_a_slash_is_found_in_the_path() {
        assert_eq!(query_hit("src/main", "/icdu/src/main.rs"), Some("src/main"));
        assert_eq!(query_hit("SRC/Main", "/icdu/src/main.rs"), Some("src/main"));
        // The match has to reach into the name: what is in a directory `src/main` is not a
        // hit, the directory is.
        assert_eq!(query_hit("src/main", "/icdu/src/main/x.rs"), None);
        assert_eq!(query_hit("src/main", "/icdu/src/main/"), Some("src/main"));
        // A `/` at the end: directories whose name ends in what comes before it.
        assert_eq!(query_hit("src/", "/icdu/src/"), Some("src/"));
        assert_eq!(query_hit("rc/", "/icdu/src/"), Some("rc/"));
        assert_eq!(query_hit("src/", "/icdu/src/main.rs"), None);
        assert_eq!(query_hit("src/", "/icdu/src"), None, "a file called src");
        // A `/` in front: from the top.
        assert_eq!(query_hit("/icdu/s", "/icdu/src/"), Some("/icdu/s"));
        assert_eq!(query_hit("/src", "/icdu/src/"), None);
        assert_eq!(query_hit("/src", "/src/"), Some("/src"));
        // The match is the one that reaches the name, not the first in the path.
        assert_eq!(query_hit("a/b", "/a/b/x/a/bc"), Some("a/b"));
        let query = Query::new("a/b");
        assert_eq!(query.highlight("/a/b/x/a/bc", 9), Some(7..10));
        // `/`: every directory, no file.
        assert_eq!(query_hit("/", "/a/b/"), Some("/"));
        assert_eq!(query_hit("/", "/a/b"), None);
    }

    #[test]
    fn a_query_with_a_star_or_question_mark_is_a_pattern_for_the_whole_name() {
        assert_eq!(query_hit("*.iso", "/dl/debian.iso"), Some("debian.iso"));
        assert_eq!(query_hit("*.iso", "/dl/DEBIAN.ISO"), Some("DEBIAN.ISO"));
        assert_eq!(query_hit("*.iso", "/dl/debian.iso.part"), None);
        assert_eq!(query_hit("*.iso", "/dl/isos/"), None);
        assert_eq!(
            query_hit("*.iso", "/dl/x.iso/"),
            Some("x.iso"),
            "a directory too"
        );
        assert_eq!(query_hit("*.iso/", "/dl/x.iso"), None, "only directories");
        assert_eq!(query_hit("*.iso/", "/dl/x.iso/"), Some("x.iso"));
        assert_eq!(query_hit("v?", "/v1"), Some("v1"));
        assert_eq!(query_hit("v?", "/v10"), None);
        assert_eq!(query_hit("übung*", "/Übung.pdf"), Some("Übung.pdf"));
        // With a `/`: the path, or a tail of it, from one component to the next.
        assert_eq!(query_hit("src/*.rs", "/icdu/src/main.rs"), Some("main.rs"));
        assert_eq!(query_hit("src/*.rs", "/icdu/src/bin/x.rs"), None);
        assert_eq!(query_hit("/*.rs", "/icdu/src/main.rs"), None);
        assert_eq!(query_hit("/icdu/*/", "/icdu/src/"), Some("src"));
    }

    fn found<'a>(name: &'a str, query: &str) -> Option<&'a str> {
        Matcher::new(query).find(name).map(|range| &name[range])
    }

    /// ASCII letters match whatever their case; the range covers exactly the matched text.
    #[test]
    fn ascii_ignores_case() {
        assert_eq!(found("Hello, World", "WORLD"), Some("World"));
        assert_eq!(found("node_modules", "ODE_MOD"), Some("ode_mod"));
        assert_eq!(found("short", "shorter"), None);
        assert_eq!(found("anything", ""), Some(""), "an empty query matches");
        assert_eq!(Matcher::new("").find("x"), Some(0..0));
        // An ASCII query inside a name with other letters, on character boundaries.
        assert_eq!(found("résumé", "SUM"), Some("sum"));
        assert_eq!(Matcher::new("sum").find("résumé"), Some(3..6));
        assert!(Matcher::new("readme").matches("README.txt"));
        // Without the accent it is a different word: no accent-insensitive matching.
        assert_eq!(found("résumé", "resume"), None);
    }

    /// Letters outside ASCII ignore case as well: umlauts, accents, Cyrillic, Greek.
    #[test]
    fn other_letters_ignore_case_too() {
        assert_eq!(found("Übung.pdf", "übung"), Some("Übung"));
        assert_eq!(found("ÜBUNG", "übung"), Some("ÜBUNG"));
        assert_eq!(found("übung", "ÜBUNG"), Some("übung"));
        assert_eq!(found("Mein RÉSUMÉ.doc", "résumé"), Some("RÉSUMÉ"));
        assert_eq!(found("Отчёт за ГОД.xlsx", "год"), Some("ГОД"));
        assert_eq!(found("отчёт", "ОТЧЁТ"), Some("отчёт"));
        assert_eq!(found("ΑΘΗΝΑ.txt", "αθηνα"), Some("ΑΘΗΝΑ"));
        // Mixed: ASCII and other letters in one query.
        assert_eq!(found("Die GRÖSSTE Datei", "größte"), None, "ß is not SS");
        assert_eq!(found("Die GRÖßTE Datei", "grö"), Some("GRÖ"));
        assert_eq!(found("plain ascii name", "ü"), None);
    }

    /// The same letter can be stored as one code point or as a letter plus a combining mark.
    /// A query in either spelling finds a name in either spelling, and the range covers the
    /// text as the name has it.
    #[test]
    fn composed_and_decomposed_spellings_match_each_other() {
        let composed = "Übung.pdf"; // U+00DC
        let decomposed = "U\u{308}bung.pdf"; // U, U+0308
        assert_eq!(composed.chars().count() + 1, decomposed.chars().count());

        // Query typed as one code point.
        assert_eq!(found(composed, "übung"), Some("Übung"));
        assert_eq!(found(decomposed, "übung"), Some("U\u{308}bung"));
        // Query typed decomposed.
        assert_eq!(found(composed, "u\u{308}bung"), Some("Übung"));
        assert_eq!(found(decomposed, "u\u{308}bung"), Some("U\u{308}bung"));

        // The highlighted range is a range of the name, not the length of the query.
        let range = Matcher::new("ü").find(decomposed).unwrap();
        assert_eq!(range, 0..3, "U and the two bytes of the mark");
        let range = Matcher::new("u\u{308}").find(composed).unwrap();
        assert_eq!(range, 0..2, "the two bytes of Ü");

        // Other letters of the table, in a word.
        assert_eq!(found("cafe\u{301} menu", "café"), Some("cafe\u{301}"));
        assert_eq!(found("Café menu", "cafe\u{301}"), Some("Café"));
        assert_eq!(
            found("PR\u{30c}I\u{301}LIS\u{30c}", "příliš"),
            Some("PR\u{30c}I\u{301}LIS\u{30c}")
        );
    }

    /// The first occurrence wins, whichever spelling it is in.
    #[test]
    fn the_earliest_match_is_reported() {
        let name = "u\u{308}ber über";
        assert_eq!(Matcher::new("über").find(name), Some(0..6));
        let name = "x über u\u{308}ber";
        assert_eq!(Matcher::new("u\u{308}ber").find(name), Some(2..7));
    }

    #[test]
    fn compose_and_decompose_are_inverse_on_the_table() {
        for &(composed, base, mark) in LETTERS {
            let split: String = [base, mark].iter().collect();
            assert_eq!(decompose(&composed.to_string()), split);
            assert_eq!(compose(&split), composed.to_string());
        }
        assert_eq!(compose("plain"), "plain");
        assert_eq!(decompose("plain"), "plain");
        assert_eq!(compose("a\u{308}bc\u{327}"), "äbç");
    }

    /// A character that folds to several is matched whole or not at all.
    #[test]
    fn a_match_never_ends_inside_a_character() {
        // `İ` (dotted capital I) lower-cases to `i` plus a combining dot above.
        let name = "İstanbul";
        assert_eq!(found(name, "i\u{307}stanbul"), Some("İstanbul"));
        let range = Matcher::new("i\u{307}s").find(name).unwrap();
        assert!(name.is_char_boundary(range.start) && name.is_char_boundary(range.end));
    }
}
