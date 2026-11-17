//! Matching a search query against entry names, ignoring case.
//!
//! Almost all names and queries are ASCII, and for those the match is a byte comparison that
//! ignores ASCII case. A query with other letters takes a slower path that folds case with
//! `char::to_lowercase`, so `übung` finds `Übung.pdf`, and that also tries the query in its
//! other Unicode spelling: `ü` can be one code point (composed) or `u` followed by a combining
//! diaeresis (decomposed, which is how macOS file systems commonly store it).

use std::ops::Range;

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
