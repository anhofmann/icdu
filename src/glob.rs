//! Shell-style patterns for `--exclude`, matched the way ncdu matches them.
//!
//! ncdu hands each pattern to `fnmatch(3)` without flags, and tries it against the whole path
//! of an entry and against every tail of that path that starts after a `/`. So `sub` excludes
//! every entry called `sub`, `a/sub` only one inside a directory `a`, and a pattern starting
//! with `/` matches from the root of the filesystem. Without flags, `*` and `?` match a `/`
//! like any other character: `sub/*` excludes everything directly in a directory `sub`.
//!
//! A pattern is compiled once ([`Pattern::new`]) and matched without recursion and without
//! allocating: a `*` is handled by remembering where it stood and coming back to it, which
//! is linear in practice. The recursive matcher this replaces took exponential time on
//! patterns like `*a*a*a*a*b`.
//!
//! Names are bytes. A name that is valid UTF-8 is matched character by character, so `?`
//! stands for one character whatever its length in bytes; a byte that is not part of a valid
//! sequence counts as one character.

/// One element of a compiled pattern.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// This byte, exactly.
    Literal(u8),
    /// `?`: any one character.
    Any,
    /// `*`: any run of characters, also none.
    Star,
    /// `[...]`: one character out of (or, negated, not out of) these.
    Class { negated: bool, items: Vec<Item> },
}

/// One member of a bracket expression.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    /// `a` (a range of one) or `a-z`.
    Range(char, char),
    /// `[:alpha:]` and the like.
    Named(Named),
}

/// The character classes of POSIX that can be named inside brackets.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Named {
    Alnum,
    Alpha,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
    /// A name that is none of these: matches nothing, as in `fnmatch`.
    Unknown,
}

impl Named {
    fn new(name: &str) -> Self {
        match name {
            "alnum" => Named::Alnum,
            "alpha" => Named::Alpha,
            "blank" => Named::Blank,
            "cntrl" => Named::Cntrl,
            "digit" => Named::Digit,
            "graph" => Named::Graph,
            "lower" => Named::Lower,
            "print" => Named::Print,
            "punct" => Named::Punct,
            "space" => Named::Space,
            "upper" => Named::Upper,
            "xdigit" => Named::Xdigit,
            _ => Named::Unknown,
        }
    }

    fn contains(self, c: char) -> bool {
        match self {
            Named::Alnum => c.is_alphanumeric(),
            Named::Alpha => c.is_alphabetic(),
            Named::Blank => c == ' ' || c == '\t',
            Named::Cntrl => c.is_control(),
            Named::Digit => c.is_ascii_digit(),
            Named::Graph => !c.is_control() && !c.is_whitespace(),
            Named::Lower => c.is_lowercase(),
            Named::Print => !c.is_control(),
            Named::Punct => c.is_ascii_punctuation(),
            Named::Space => c.is_whitespace(),
            Named::Upper => c.is_uppercase(),
            Named::Xdigit => c.is_ascii_hexdigit(),
            Named::Unknown => false,
        }
    }
}

impl Item {
    fn contains(&self, c: char) -> bool {
        match *self {
            Item::Range(lo, hi) => lo <= c && c <= hi,
            Item::Named(named) => named.contains(c),
        }
    }
}

/// A compiled `--exclude` pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    tokens: Vec<Token>,
    /// The pattern is plain text without a `/`. It can then only ever equal the last
    /// component of a path, which saves trying every tail.
    plain_name: Option<Box<[u8]>>,
}

impl Pattern {
    pub fn new(pattern: &str) -> Self {
        let chars: Vec<char> = pattern.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        let literal = |tokens: &mut Vec<Token>, c: char| {
            let mut buf = [0; 4];
            tokens.extend(c.encode_utf8(&mut buf).bytes().map(Token::Literal));
        };
        while i < chars.len() {
            match chars[i] {
                '*' => {
                    // Several stars in a row match what one does.
                    if tokens.last() != Some(&Token::Star) {
                        tokens.push(Token::Star);
                    }
                }
                '?' => tokens.push(Token::Any),
                // A backslash takes the next character literally. At the very end it stands
                // for itself.
                '\\' => {
                    i += usize::from(i + 1 < chars.len());
                    literal(&mut tokens, chars[i]);
                }
                '[' => match parse_class(&chars[i..]) {
                    Some((class, len)) => {
                        tokens.push(class);
                        i += len - 1;
                    }
                    // No closing bracket: an ordinary `[`.
                    None => literal(&mut tokens, '['),
                },
                c => literal(&mut tokens, c),
            }
            i += 1;
        }
        let plain = tokens
            .iter()
            .map(|t| match t {
                Token::Literal(b) if *b != b'/' => Some(*b),
                _ => None,
            })
            .collect::<Option<Vec<u8>>>();
        Pattern {
            tokens,
            plain_name: plain.map(Vec::into_boxed_slice),
        }
    }

    /// True if the pattern matches all of `text`.
    pub fn matches(&self, text: &[u8]) -> bool {
        let tokens = &self.tokens;
        let (mut t, mut s) = (0, 0);
        // Where the last `*` stood, and how much of the text it has taken so far. When the
        // tokens after it fail to match, it takes one character more and they try again.
        let mut star: Option<(usize, usize)> = None;
        loop {
            let step = match tokens.get(t) {
                Some(Token::Star) => {
                    star = Some((t, s));
                    t += 1;
                    continue;
                }
                None if s == text.len() => return true,
                None => None,
                Some(_) if s == text.len() => None,
                Some(Token::Literal(byte)) => (text[s] == *byte).then_some(1),
                Some(Token::Any) => Some(char_len(&text[s..])),
                Some(Token::Class { negated, items }) => {
                    let (c, len) = decode(&text[s..]);
                    let inside = c.is_some_and(|c| items.iter().any(|item| item.contains(c)));
                    (inside != *negated).then_some(len)
                }
            };
            match (step, star) {
                (Some(len), _) => {
                    t += 1;
                    s += len;
                }
                (None, Some((star_t, taken))) if taken < text.len() => {
                    let taken = taken + char_len(&text[taken..]);
                    star = Some((star_t, taken));
                    t = star_t + 1;
                    s = taken;
                }
                (None, _) => return false,
            }
        }
    }

    /// True if the pattern excludes the entry `name` in the directory `dir`, by ncdu's rule:
    /// it matches the entry's whole path, or a tail of it that starts after a `/`.
    ///
    /// `path` is scratch space for the whole path, reused between calls.
    pub fn excludes(&self, dir: &[u8], name: &[u8], path: &mut Vec<u8>) -> bool {
        if let Some(plain) = &self.plain_name {
            return **plain == *name;
        }
        path.clear();
        path.extend_from_slice(dir);
        if !dir.ends_with(b"/") {
            path.push(b'/');
        }
        path.extend_from_slice(name);
        if self.matches(path) {
            return true;
        }
        // A pattern that starts with a star has already tried every tail: the star takes
        // whatever comes before one.
        if matches!(self.tokens.first(), Some(Token::Star)) {
            return false;
        }
        (0..path.len())
            .filter(|&i| path[i] == b'/' && path.get(i + 1).is_some_and(|&next| next != b'/'))
            .any(|i| self.matches(&path[i + 1..]))
    }
}

/// Parse the bracket expression at the start of `chars` (which begins with `[`). Returns the
/// class and the number of characters it took, or `None` if it has no closing bracket.
fn parse_class(chars: &[char]) -> Option<(Token, usize)> {
    let mut i = 1;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    i += usize::from(negated);
    let mut items = Vec::new();
    let first = i;
    loop {
        let c = *chars.get(i)?;
        // A `]` right at the start is a member, not the end.
        if c == ']' && i > first {
            return Some((Token::Class { negated, items }, i + 1));
        }
        // `[:alpha:]`. Without its end it is just a `[`.
        if c == '[' && chars.get(i + 1) == Some(&':') {
            let end = (i + 2..chars.len().saturating_sub(1))
                .find(|&e| chars[e] == ':' && chars[e + 1] == ']');
            if let Some(end) = end {
                let name: String = chars[i + 2..end].iter().collect();
                items.push(Item::Named(Named::new(&name)));
                i = end + 2;
                continue;
            }
        }
        let lo = if c == '\\' {
            i += 1;
            *chars.get(i)?
        } else {
            c
        };
        // `a-z`, unless the `-` is the last character before the bracket.
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&c| c != ']') {
            let mut hi = chars[i + 2];
            i += 2;
            if hi == '\\' {
                i += 1;
                hi = *chars.get(i)?;
            }
            items.push(Item::Range(lo, hi));
        } else {
            items.push(Item::Range(lo, lo));
        }
        i += 1;
    }
}

/// The character at the start of `bytes` and its length in bytes. A byte that does not start
/// a valid UTF-8 sequence is one character that equals nothing.
fn decode(bytes: &[u8]) -> (Option<char>, usize) {
    let len = match bytes.first() {
        Some(&byte) if byte < 0x80 => return (Some(char::from(byte)), 1),
        Some(0xC0..=0xDF) => 2,
        Some(0xE0..=0xEF) => 3,
        Some(0xF0..=0xF7) => 4,
        _ => 1,
    };
    match bytes.get(..len).map(std::str::from_utf8) {
        Some(Ok(text)) => (text.chars().next(), len),
        _ => (None, 1),
    }
}

fn char_len(bytes: &[u8]) -> usize {
    match bytes.first() {
        Some(&byte) if byte < 0x80 => 1,
        _ => decode(bytes).1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, text: &str) -> bool {
        Pattern::new(pattern).matches(text.as_bytes())
    }

    #[test]
    fn basics() {
        assert!(matches("node_modules", "node_modules"));
        assert!(!matches("node_modules", "node_module"));
        assert!(!matches("node_modules", "node_modules2"));
        assert!(matches("*.log", "a.log"));
        assert!(matches("*.log", ".log"));
        assert!(!matches("*.log", "a.logs"));
        assert!(matches("*cache*", "my_cache_dir"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a?c", "abbc"));
        assert!(matches("", ""));
        assert!(!matches("", "a"));
        assert!(matches("*", ""));
        assert!(matches("**", "anything"));
        assert!(matches("a*b*c", "a-b-b-c"));
        assert!(!matches("a*b*c", "a-b-b"));
    }

    /// Without flags, `fnmatch` gives the slash no special role.
    #[test]
    fn wildcards_match_slashes() {
        assert!(matches("sub/*", "sub/deep/file"));
        assert!(matches("a?b", "a/b"));
        assert!(matches("*.o", "dir/x.o"));
        assert!(matches("[a-c/]x", "/x"));
    }

    #[test]
    fn classes() {
        assert!(matches("file[12]", "file1"));
        assert!(!matches("file[12]", "file3"));
        assert!(matches("file[!1]", "file2"));
        assert!(!matches("file[!1]", "file1"));
        assert!(matches("file[^1]", "file2"));
        assert!(matches("[Ff]ile?", "File3"));
        assert!(matches("[a-c]", "b"));
        assert!(!matches("[a-c]", "d"));
        assert!(matches("x[a-cx-z]", "xy"));
        // A bracket right after the opening one is a member; so is a trailing dash.
        assert!(matches("[]]", "]"));
        assert!(matches("[!]]", "a"));
        assert!(!matches("[!]]", "]"));
        assert!(matches("[a-]", "-"));
        assert!(matches("[a-]", "a"));
        // Named classes, alone and next to other members.
        assert!(matches("[[:alpha:]]", "a"));
        assert!(!matches("[[:alpha:]]", "1"));
        assert!(matches("file[[:digit:]]", "file1"));
        assert!(matches("[[:upper:]]ile3", "File3"));
        assert!(!matches("[[:upper:]]ile1", "file1"));
        assert!(matches("[![:alpha:]]x", "#x"));
        assert!(matches("[[:digit:]x-z]", "y"));
        assert!(matches("[[:alpha:]]", "ä"));
        assert!(!matches("[[:nonsense:]]", "a"));
        assert!(
            matches("[[:alpha]", ":"),
            "no `:]`, so these are five members"
        );
        // An unterminated class is an ordinary `[`.
        assert!(matches("a[b", "a[b"));
        assert!(!matches("a[b", "ab"));
        assert!(matches("[", "["));
    }

    #[test]
    fn backslash_takes_the_next_character_literally() {
        assert!(matches(r"a\*b", "a*b"));
        assert!(!matches(r"a\*b", "axb"));
        assert!(matches(r"a\/sub", "a/sub"));
        assert!(matches(r"fil\e1", "file1"));
        assert!(matches(r"\[x\]", "[x]"));
        assert!(matches(r"[\]]", "]"));
        assert!(matches(r"end\", r"end\"));
    }

    /// `?` and a class stand for one character, however many bytes it has; names that are
    /// not valid UTF-8 are matched byte by byte.
    #[test]
    fn characters_not_bytes() {
        assert!(matches("f?le", "fële"));
        assert!(matches("?", "日"));
        assert!(!matches("?", "日本"));
        assert!(matches("[ä-ü]x", "öx"));
        assert!(matches("*.jpg", "写真.jpg"));
        let p = Pattern::new("a?c");
        assert!(p.matches(b"a\xffc"));
        assert!(!p.matches(b"a\xff\xfec"));
        assert!(Pattern::new("a*c").matches(b"a\xff\xfec"));
        assert!(!Pattern::new("a[b-z]c").matches(b"a\xffc"));
        assert!(Pattern::new("a[!b-z]c").matches(b"a\xffc"));
    }

    /// The pattern that took the old, recursive matcher exponential time.
    #[test]
    fn many_stars_do_not_blow_up() {
        let name = "a".repeat(200);
        let pattern = Pattern::new(&format!("{}b", "*a".repeat(30)));
        let start = std::time::Instant::now();
        for _ in 0..100 {
            assert!(!pattern.matches(name.as_bytes()));
        }
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "100 matches took {:?}",
            start.elapsed()
        );
        assert!(Pattern::new(&format!("{}*", "*a".repeat(30))).matches(name.as_bytes()));
    }

    fn excludes(pattern: &str, dir: &str, name: &str) -> bool {
        Pattern::new(pattern).excludes(dir.as_bytes(), name.as_bytes(), &mut Vec::new())
    }

    /// The whole path, and every tail of it that starts after a slash.
    #[test]
    fn a_pattern_is_tried_against_the_path_and_its_tails() {
        assert!(excludes("sub", "/r/a", "sub"));
        assert!(excludes("a/sub", "/r/a", "sub"));
        assert!(!excludes("a/sub", "/r/b", "sub"));
        assert!(excludes("r/a/sub", "/r/a", "sub"));
        assert!(excludes("/r/a/sub", "/r/a", "sub"));
        assert!(
            !excludes("/a/sub", "/r/a", "sub"),
            "a leading slash anchors at the root"
        );
        assert!(!excludes("su", "/r/a", "sub"));
        assert!(
            !excludes("ub", "/r/a", "sub"),
            "a tail starts after a slash, not anywhere"
        );
        assert!(excludes("sub/*", "/r/a/sub", "file"));
        assert!(!excludes("sub/*", "/r/a", "sub"));
        // A directory path that ends in a slash (the root of the filesystem).
        assert!(excludes("/proc", "/", "proc"));
        assert!(excludes("proc", "/", "proc"));
        // Plain names only ever equal the last component.
        assert!(!excludes("a", "/r/a", "sub"));
        // A leading star reaches back over the directories, with or without tails.
        assert!(excludes("*b", "/r/a", "sub"));
        assert!(excludes("*/sub", "/r/a", "sub"));
        assert!(excludes("*a/s?b", "/r/a", "sub"));
        assert!(!excludes("*x/sub", "/r/a", "sub"));
    }
}
