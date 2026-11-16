//! ncdu-compatible JSON import/export.
//!
//! Format (ncdu export version 1): a top-level array
//!     `[major, minor, metadata, root]`
//! where each directory is itself an array `[info, child, child, ...]` beginning with its own
//! info object, and files are plain info objects. Sizes are stored *per item* (`asize`/`dsize`
//! are the entry's own size); totals are recomputed on load.
//!
//! Both directions are written by hand and stream: export writes entry by entry, import parses
//! entry by entry into the tree (see [`Parser`]). Neither holds the dump in memory, neither
//! recurses, and names pass through as bytes, which a JSON library would not allow.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use compact_str::CompactString;

use crate::model::{Excluded, NodeIdx, NodeKind, Tree};

const MAJOR: u32 = 1;
const MINOR: u32 = 2;

// ---- Export ---------------------------------------------------------------------------------

pub fn export<W: Write>(tree: &Tree, w: &mut W) -> io::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    write!(
        w,
        "[{MAJOR},{MINOR},{{\"progname\":\"icdu\",\"progver\":\"{}\",\"timestamp\":{ts}}},",
        env!("CARGO_PKG_VERSION")
    )?;
    write_node(tree, tree.root, w)?;
    writeln!(w, "]")
}

/// Write the entry `idx`; a directory with everything below it, as `[info, child, ...]`.
/// The directories being written are kept on a stack on the heap, each with the position of
/// its next child, so a very deep tree cannot exhaust the call stack.
fn write_node<W: Write>(tree: &Tree, idx: NodeIdx, w: &mut W) -> io::Result<()> {
    if !tree.nodes[idx].is_dir() {
        return write_item(tree, idx, w);
    }
    w.write_all(b"[")?;
    write_item(tree, idx, w)?;
    let mut open: Vec<(NodeIdx, usize)> = vec![(idx, 0)];
    while let Some((dir, next)) = open.last_mut() {
        let Some(&child) = tree.nodes[*dir].children.get(*next) else {
            w.write_all(b"]")?;
            open.pop();
            continue;
        };
        *next += 1;
        w.write_all(b",")?;
        if tree.nodes[child].is_dir() {
            w.write_all(b"[")?;
            write_item(tree, child, w)?;
            open.push((child, 0));
        } else {
            write_item(tree, child, w)?;
        }
    }
    Ok(())
}

fn write_item<W: Write>(tree: &Tree, idx: NodeIdx, w: &mut W) -> io::Result<()> {
    let n = &tree.nodes[idx];
    write!(w, "{{\"name\":")?;
    write_json_string(tree.name_bytes(idx), w)?;
    write!(
        w,
        ",\"asize\":{},\"dsize\":{},\"ino\":{},\"dev\":{}",
        tree.own_apparent(idx),
        tree.own_disk(idx),
        n.ino,
        n.dev
    )?;
    if n.hlink {
        write!(w, ",\"hlnkc\":true")?;
    }
    if matches!(n.kind, NodeKind::Link | NodeKind::Other) {
        write!(w, ",\"notreg\":true")?;
    }
    // A directory whose listing has not arrived yet (a dump taken while the scan is still
    // running) is written as a read error too. The format has no word for "not read yet", and
    // this one makes every reader show the directory as incomplete instead of as empty.
    if n.read_error || tree.is_unread(idx) {
        write!(w, ",\"read_error\":true")?;
    }
    match n.excluded {
        Excluded::Pattern => write!(w, ",\"excluded\":\"pattern\"")?,
        Excluded::OtherFs => write!(w, ",\"excluded\":\"otherfs\"")?,
        Excluded::Age => write!(w, ",\"excluded\":\"age\"")?,
        Excluded::No => {}
    }
    w.write_all(b"}")
}

/// Write a name as a JSON string. The format stores names "in the same encoding as reported
/// by the filesystem": the bytes go out as they are, with only the characters JSON reserves
/// escaped. A name that is not valid UTF-8 therefore produces a string that is not valid
/// UTF-8 either, exactly as ncdu writes it.
fn write_json_string<W: Write>(name: &[u8], w: &mut W) -> io::Result<()> {
    w.write_all(b"\"")?;
    // Runs of ordinary bytes are written in one go.
    let mut plain = 0;
    for (i, &byte) in name.iter().enumerate() {
        let escape: Option<&[u8]> = match byte {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            b'\t' => Some(b"\\t"),
            byte if byte < 0x20 => None,
            _ => continue,
        };
        w.write_all(&name[plain..i])?;
        match escape {
            Some(escape) => w.write_all(escape)?,
            None => write!(w, "\\u{byte:04x}")?,
        }
        plain = i + 1;
    }
    w.write_all(&name[plain..])?;
    w.write_all(b"\"")
}

// ---- Import ---------------------------------------------------------------------------------

/// Read a dump into a tree.
///
/// The dump is parsed as it is read: each entry goes straight into the tree, and nothing of
/// the file is kept besides. Open directories are tracked on an explicit stack, so the depth
/// of the tree is limited by memory only.
pub fn import<R: Read>(r: R) -> io::Result<Tree> {
    let mut p = Parser::new(r);
    p.expect(b'[')?;
    // Format version. Unknown fields are skipped, so a higher minor version still loads.
    p.uint()?;
    p.expect(b',')?;
    p.uint()?;
    p.expect(b',')?;
    if p.peek_token()? != b'{' {
        return Err(p.error("expected the metadata object"));
    }
    p.skip_value()?;
    p.expect(b',')?;

    // The root must be a directory array: [info, children...].
    if p.peek_token()? != b'[' {
        return Err(p.error("expected the root directory, an array"));
    }
    p.bump();
    let mut item = Item::default();
    p.info(&mut item)?;
    if let Err(e) = check_root_name(&item.name) {
        return Err(p.error(e));
    }
    let (name, raw) = display_name(&item.name);
    let mut tree = Tree::new_imported(name, item.apparent, item.disk, item.dev, item.ino);
    if let Some(raw) = raw {
        tree.set_raw_name(tree.root, raw);
    }

    // Dedup hard links during import too, so totals match a live scan.
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    // The directories that are still open, innermost last.
    let mut open = vec![tree.root];
    while let Some(&parent) = open.last() {
        match p.peek_token()? {
            b']' => {
                p.bump();
                open.pop();
            }
            b',' => {
                p.bump();
                let is_dir = match p.peek_token()? {
                    b'[' => {
                        p.bump();
                        true
                    }
                    b'{' => false,
                    _ => return Err(p.error("expected an entry: `{` or `[`")),
                };
                p.info(&mut item)?;
                if let Err(e) = check_name(&item.name) {
                    return Err(p.error(e));
                }
                let kind = match (is_dir, item.notreg) {
                    (true, _) => NodeKind::Dir,
                    (false, true) => NodeKind::Other,
                    (false, false) => NodeKind::File,
                };
                // Re-derive hard-link sharing so imported totals dedupe like a live scan.
                let shared = !is_dir && item.hlink && !seen.insert((item.dev, item.ino));
                let (name, raw) = display_name(&item.name);
                let idx = tree.import_child(
                    parent,
                    name,
                    kind,
                    item.apparent,
                    item.disk,
                    item.dev,
                    item.ino,
                    item.hlink,
                    shared,
                    item.excluded,
                    item.read_error,
                );
                if let Some(raw) = raw {
                    tree.set_raw_name(idx, raw);
                }
                if is_dir {
                    open.push(idx);
                }
            }
            _ => return Err(p.error("expected `,` or `]`")),
        }
    }
    p.expect(b']')?;
    if p.peek_token_or_end()?.is_some() {
        return Err(p.error("unexpected data after the end of the dump"));
    }
    Ok(tree)
}

/// The fields of one entry's info object that the tree uses.
#[derive(Default)]
struct Item {
    /// The name as the dump has it: bytes in the encoding of the filesystem they came from.
    name: Vec<u8>,
    has_name: bool,
    apparent: u64,
    disk: u64,
    dev: u64,
    ino: u64,
    hlink: bool,
    notreg: bool,
    read_error: bool,
    excluded: Excluded,
}

/// A name for the tree: the text to show, and the real bytes if they are not valid UTF-8.
fn display_name(bytes: &[u8]) -> (CompactString, Option<Box<[u8]>>) {
    match std::str::from_utf8(bytes) {
        Ok(name) => (name.into(), None),
        Err(_) => (
            String::from_utf8_lossy(bytes).as_ref().into(),
            Some(bytes.into()),
        ),
    }
}

/// A pull parser for exactly the dump format: nested arrays whose elements are flat objects
/// with scalar fields. It is not a general JSON parser. What it does beyond the grammar:
///
/// * Strings are bytes. ncdu writes names as the filesystem reports them, so a dump may hold
///   strings that are not valid UTF-8, which a JSON library refuses.
/// * It reads through a fixed buffer and keeps nothing, so memory use does not depend on the
///   size of the dump.
/// * Nothing recurses: values to skip are tracked with a stack on the heap.
/// * Errors name the byte offset they occurred at.
struct Parser<R> {
    src: R,
    buf: Box<[u8]>,
    /// Read position in `buf`.
    pos: usize,
    /// Number of valid bytes in `buf`.
    len: usize,
    /// Bytes of input that came before `buf`.
    before: u64,
    /// Reused for object keys and for strings that are skipped.
    scratch: Vec<u8>,
}

impl<R: Read> Parser<R> {
    fn new(src: R) -> Self {
        Parser {
            src,
            buf: vec![0; 64 * 1024].into_boxed_slice(),
            pos: 0,
            len: 0,
            before: 0,
            scratch: Vec::new(),
        }
    }

    fn error(&self, msg: impl std::fmt::Display) -> io::Error {
        bad(&format!(
            "invalid dump at byte {}: {msg}",
            self.before + self.pos as u64
        ))
    }

    /// The next byte without consuming it; `None` at the end of the input.
    fn peek(&mut self) -> io::Result<Option<u8>> {
        while self.pos == self.len {
            self.before += self.len as u64;
            self.pos = 0;
            self.len = 0;
            match self.src.read(&mut self.buf) {
                Ok(0) => return Ok(None),
                Ok(n) => self.len = n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Some(self.buf[self.pos]))
    }

    /// Consume the byte `peek` just returned.
    fn bump(&mut self) {
        self.pos += 1;
    }

    /// The next byte, consumed; the end of the input is an error.
    fn next(&mut self) -> io::Result<u8> {
        match self.peek()? {
            Some(byte) => {
                self.bump();
                Ok(byte)
            }
            None => Err(self.error("unexpected end of input")),
        }
    }

    /// The next byte that is not whitespace, not consumed; `None` at the end of the input.
    fn peek_token_or_end(&mut self) -> io::Result<Option<u8>> {
        loop {
            match self.peek()? {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.bump(),
                other => return Ok(other),
            }
        }
    }

    /// The next byte that is not whitespace, not consumed.
    fn peek_token(&mut self) -> io::Result<u8> {
        match self.peek_token_or_end()? {
            Some(byte) => Ok(byte),
            None => Err(self.error("unexpected end of input")),
        }
    }

    /// Consume the next token, which must be the punctuation `want`.
    fn expect(&mut self, want: u8) -> io::Result<()> {
        if self.peek_token()? != want {
            return Err(self.error(format_args!("expected `{}`", want as char)));
        }
        self.bump();
        Ok(())
    }

    /// A string, from its opening quote to its closing one, appended to `out` with the
    /// escapes decoded. Bytes outside ASCII are taken as they are and not checked.
    fn string(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        if self.peek_token()? != b'"' {
            return Err(self.error("expected a string"));
        }
        self.bump();
        loop {
            // Whole runs of ordinary bytes are copied straight from the buffer.
            if self.peek()?.is_none() {
                return Err(self.error("unexpected end of input"));
            }
            let run = self.buf[self.pos..self.len]
                .iter()
                .position(|&b| b == b'"' || b == b'\\' || b < 0x20)
                .unwrap_or(self.len - self.pos);
            out.extend_from_slice(&self.buf[self.pos..self.pos + run]);
            self.pos += run;
            if self.pos == self.len {
                continue;
            }
            match self.buf[self.pos] {
                b'"' => {
                    self.bump();
                    return Ok(());
                }
                b'\\' => {
                    self.bump();
                    self.escape(out)?;
                }
                _ => return Err(self.error("control character in a string")),
            }
        }
    }

    /// What follows a backslash in a string.
    fn escape(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let byte = match self.next()? {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let first = self.hex4()?;
                let code = match first {
                    // A high surrogate needs its low half to make a character.
                    0xD800..=0xDBFF => {
                        let paired = self.peek()? == Some(b'\\') && {
                            self.bump();
                            self.next()? == b'u'
                        };
                        if !paired {
                            return Err(self.error("unpaired surrogate in a \\u escape"));
                        }
                        match self.hex4()? {
                            low @ 0xDC00..=0xDFFF => {
                                0x10000 + ((first - 0xD800) << 10) + (low - 0xDC00)
                            }
                            _ => return Err(self.error("unpaired surrogate in a \\u escape")),
                        }
                    }
                    0xDC00..=0xDFFF => return Err(self.error("unpaired surrogate in a \\u escape")),
                    code => code,
                };
                let Some(c) = char::from_u32(code) else {
                    return Err(self.error("invalid \\u escape"));
                };
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                return Ok(());
            }
            _ => return Err(self.error("invalid escape in a string")),
        };
        out.push(byte);
        Ok(())
    }

    /// The four hex digits of a `\u` escape.
    fn hex4(&mut self) -> io::Result<u32> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = match self.next()? {
                b @ b'0'..=b'9' => b - b'0',
                b @ b'a'..=b'f' => b - b'a' + 10,
                b @ b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.error("invalid \\u escape")),
            };
            value = value * 16 + u32::from(digit);
        }
        Ok(value)
    }

    /// A non-negative integer, the only kind of number the format uses for sizes, devices and
    /// inodes. A sign, a fraction or an exponent is an error, and so is a value above `u64`.
    fn uint(&mut self) -> io::Result<u64> {
        const WANT: &str = "expected a non-negative integer";
        if !self.peek_token()?.is_ascii_digit() {
            return Err(self.error(WANT));
        }
        let mut value = 0u64;
        while let Some(digit @ b'0'..=b'9') = self.peek()? {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(digit - b'0')))
                .ok_or_else(|| self.error("number too large"))?;
            self.bump();
        }
        if matches!(self.peek()?, Some(b'.' | b'e' | b'E')) {
            return Err(self.error(WANT));
        }
        Ok(value)
    }

    fn boolean(&mut self) -> io::Result<bool> {
        match self.peek_token()? {
            b't' => self.word(b"true").map(|()| true),
            b'f' => self.word(b"false").map(|()| false),
            _ => Err(self.error("expected true or false")),
        }
    }

    /// Consume exactly the bytes of `word`.
    fn word(&mut self, word: &[u8]) -> io::Result<()> {
        for &want in word {
            if self.peek()? != Some(want) {
                return Err(self.error("invalid value"));
            }
            self.bump();
        }
        Ok(())
    }

    /// Skip one value of any shape: the metadata object, or a field this version does not
    /// know. Nested arrays and objects are followed with a stack of their closing brackets.
    fn skip_value(&mut self) -> io::Result<()> {
        let mut closers: Vec<u8> = Vec::new();
        loop {
            match self.peek_token()? {
                b'{' => {
                    self.bump();
                    closers.push(b'}');
                }
                b'[' => {
                    self.bump();
                    closers.push(b']');
                }
                close @ (b'}' | b']') => {
                    if closers.pop() != Some(close) {
                        return Err(self.error("mismatched bracket"));
                    }
                    self.bump();
                }
                b'"' => {
                    let mut scratch = std::mem::take(&mut self.scratch);
                    scratch.clear();
                    let skipped = self.string(&mut scratch);
                    self.scratch = scratch;
                    skipped?;
                }
                b't' => self.word(b"true")?,
                b'f' => self.word(b"false")?,
                b'n' => self.word(b"null")?,
                b'-' | b'0'..=b'9' => {
                    while let Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') = self.peek()? {
                        self.bump();
                    }
                }
                // Separators inside the value being skipped.
                b',' | b':' if !closers.is_empty() => self.bump(),
                _ => return Err(self.error("invalid value")),
            }
            if closers.is_empty() {
                return Ok(());
            }
        }
    }

    /// One entry's info object, `{"name": ..., ...}`, into `item`. Known fields must have
    /// the type the format gives them; unknown fields are skipped.
    fn info(&mut self, item: &mut Item) -> io::Result<()> {
        *item = Item {
            name: std::mem::take(&mut item.name),
            ..Item::default()
        };
        item.name.clear();
        self.expect(b'{')?;
        if self.peek_token()? == b'}' {
            self.bump();
        } else {
            loop {
                let mut key = std::mem::take(&mut self.scratch);
                key.clear();
                let read = self.string(&mut key);
                let known = self.field(&key, item);
                self.scratch = key;
                read?;
                known?;
                match self.peek_token()? {
                    b',' => self.bump(),
                    b'}' => {
                        self.bump();
                        break;
                    }
                    _ => return Err(self.error("expected `,` or `}`")),
                }
            }
        }
        if !item.has_name {
            return Err(self.error("entry has no name"));
        }
        Ok(())
    }

    /// The value of the field `key`, after its colon.
    fn field(&mut self, key: &[u8], item: &mut Item) -> io::Result<()> {
        self.expect(b':')?;
        match key {
            b"name" => {
                item.name.clear();
                item.has_name = true;
                self.string(&mut item.name)
            }
            b"asize" => self.uint().map(|v| item.apparent = v),
            b"dsize" => self.uint().map(|v| item.disk = v),
            b"dev" => self.uint().map(|v| item.dev = v),
            b"ino" => self.uint().map(|v| item.ino = v),
            b"hlnkc" => self.boolean().map(|v| item.hlink = v),
            b"notreg" => self.boolean().map(|v| item.notreg = v),
            b"read_error" => self.boolean().map(|v| item.read_error = v),
            b"excluded" => {
                let mut reason = Vec::new();
                self.string(&mut reason)?;
                item.excluded = match reason.as_slice() {
                    b"otherfs" => Excluded::OtherFs,
                    b"age" => Excluded::Age,
                    _ => Excluded::Pattern,
                };
                Ok(())
            }
            _ => self.skip_value(),
        }
    }
}

/// Names become real paths (`Tree::path_of`) that delete, open and the shell act on, so a
/// crafted dump must not be able to make an entry resolve anywhere but where it is shown. A
/// child name is exactly one path component: non-empty, not `.` or `..`, no `/`, no NUL.
fn check_name(name: &[u8]) -> Result<(), String> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(format!(
            "invalid entry name {:?}",
            String::from_utf8_lossy(name)
        ));
    }
    Ok(())
}

/// The root name is a whole path, but it still may not be empty, contain a NUL, or climb out
/// with a `..` component.
fn check_root_name(name: &[u8]) -> Result<(), String> {
    if name.is_empty() || name.contains(&0) || name.split(|&b| b == b'/').any(|c| c == b"..") {
        return Err(format!(
            "invalid root name {:?}",
            String::from_utf8_lossy(name)
        ));
    }
    Ok(())
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names go out byte for byte: a name that is not valid UTF-8 keeps its real bytes, as in
    /// a dump written by ncdu, and only what JSON reserves is escaped.
    #[test]
    fn export_writes_names_byte_for_byte() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let add = |t: &mut Tree, name: &str| {
            t.import_child(
                t.root,
                name.into(),
                NodeKind::File,
                1,
                1,
                1,
                1,
                false,
                false,
                Excluded::No,
                false,
            )
        };
        let raw = add(&mut t, "bad_\u{FFFD}name");
        t.set_raw_name(raw, Box::from(&b"bad_\xffname"[..]));
        add(&mut t, "q\"uote\\back\nline\u{1}ctl");
        add(&mut t, "ünï");

        let mut buf = Vec::new();
        export(&t, &mut buf).unwrap();
        let has = |needle: &[u8]| buf.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"\"name\":\"bad_\xffname\""), "the raw byte");
        assert!(!has("\u{FFFD}".as_bytes()), "not the replacement character");
        assert!(has(br#""name":"q\"uote\\back\nline\u0001ctl""#), "escapes");
        assert!(has("\"name\":\"ünï\"".as_bytes()), "UTF-8 passes through");
    }

    #[test]
    fn export_then_import_roundtrips_totals() {
        // Build a small tree by hand via the import path, export it, re-import, compare totals.
        let mut t = Tree::new_imported("/root".into(), 4096, 4096, 1, 2);
        let d = t.import_child(
            t.root,
            "sub".into(),
            NodeKind::Dir,
            4096,
            4096,
            1,
            3,
            false,
            false,
            Excluded::No,
            false,
        );
        t.import_child(
            d,
            "a".into(),
            NodeKind::File,
            1000,
            2048,
            1,
            4,
            false,
            false,
            Excluded::No,
            false,
        );
        t.import_child(
            t.root,
            "b".into(),
            NodeKind::File,
            500,
            512,
            1,
            5,
            false,
            false,
            Excluded::No,
            false,
        );
        let total = t.nodes[t.root].apparent;

        let mut buf = Vec::new();
        export(&t, &mut buf).unwrap();
        let t2 = import(&buf[..]).unwrap();

        assert_eq!(t2.nodes[t2.root].apparent, total);
        assert_eq!(t2.total_files, t.total_files);
        assert_eq!(t2.total_dirs, t.total_dirs);
        assert_eq!(t2.nodes[t2.root].name, "/root");
        assert_eq!(t2.nodes[t2.root].items, 3, "sub, sub/a, b");
    }

    /// The format stores no item counts; they are derived on import, counting hard-link
    /// duplicates and excluded entries like a live scan does.
    /// Dumps name this program in their header, and a dump written under any other program
    /// name (ncdu, or rcdu as this project was called up to 0.4.1) loads all the same.
    #[test]
    fn header_names_icdu_and_any_progname_is_accepted() {
        let t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let mut out = Vec::new();
        export(&t, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with(r#"[1,2,{"progname":"icdu","progver":""#),
            "{text}"
        );

        for prog in ["icdu", "rcdu", "ncdu", "something else"] {
            let json = format!(r#"[1,2,{{"progname":"{prog}"}},[{{"name":"/r"}}]]"#);
            assert!(import(json.as_bytes()).is_ok(), "{prog}");
        }
    }

    #[test]
    fn import_derives_item_counts() {
        let json = r#"[1,2,{"progname":"ncdu"},
            [{"name":"/r"},
                [{"name":"d"},
                    {"name":"f1","asize":10,"ino":7,"hlnkc":true},
                    {"name":"f2","asize":10,"ino":7,"hlnkc":true},
                    [{"name":"e"}]
                ],
                [{"name":"node_modules","excluded":"pattern"}],
                {"name":"g","asize":5}
            ]]"#;
        let t = import(json.as_bytes()).unwrap();
        let find = |name: &str| t.nodes.iter().position(|n| n.name == name).unwrap();
        assert!(t.nodes[find("f2")].shared, "second link is a duplicate");
        assert_eq!(t.nodes[find("d")].items, 3, "f1, f2, e");
        assert_eq!(t.nodes[find("e")].items, 0);
        assert_eq!(t.nodes[find("node_modules")].items, 0);
        assert_eq!(t.nodes[t.root].items, 6, "d (+3), node_modules, g");
    }

    /// A dump `depth` directories deep, with one file at the bottom.
    fn deep_dump(depth: usize) -> String {
        let mut json = String::from("[1,2,{},");
        for level in 0..depth {
            json.push_str(if level == 0 {
                r#"[{"name":"/r"},"#
            } else {
                r#"[{"name":"d"},"#
            });
        }
        json.push_str(r#"{"name":"leaf","asize":7}"#);
        json.push_str(&"]".repeat(depth));
        json.push(']');
        json
    }

    /// Depth is limited by memory only: no recursion on the call stack, no nesting limit in
    /// the parser. (serde_json stopped at 128 levels, so not even our own export of a deep
    /// tree could be read back.) The export side must cope with the same tree.
    #[test]
    fn import_and_export_handle_very_deep_trees() {
        for depth in [126, 10_000] {
            let t = import(deep_dump(depth).as_bytes()).unwrap();
            assert_eq!(t.total_dirs, depth as u64, "depth {depth}");
            assert_eq!(t.total_files, 1);
            assert_eq!(t.nodes[t.root].apparent, 7);
            let leaf = t.nodes.len() - 1;
            assert_eq!(t.nodes[leaf].name, "leaf");
            let mut levels = 0;
            let mut cur = leaf;
            while let Some(p) = t.nodes[cur].parent {
                cur = p;
                levels += 1;
            }
            assert_eq!(levels, depth);

            let mut out = Vec::new();
            export(&t, &mut out).unwrap();
            let again = import(&out[..]).unwrap();
            assert_eq!(again.total_dirs, depth as u64);
            assert_eq!(again.nodes[again.root].apparent, 7);
        }
    }

    /// A name that is not valid UTF-8 is stored in a dump with its real bytes (that is how
    /// ncdu writes it). It imports, is shown lossily, and goes out again byte for byte.
    #[test]
    fn import_keeps_the_bytes_of_names_that_are_not_utf8() {
        let mut json = Vec::new();
        json.extend_from_slice(b"[1,2,{\"progname\":\"ncdu\"},[{\"name\":\"/r\xfe\"},");
        json.extend_from_slice(b"{\"name\":\"bad_\xff\\\"name\",\"asize\":3},");
        json.extend_from_slice(b"[{\"name\":\"dir_\xc3\x28\"},{\"name\":\"ok\"}]]]");
        let t = import(&json[..]).unwrap();

        let find = |name: &str| t.nodes.iter().position(|n| n.name == name).unwrap();
        let file = find("bad_\u{FFFD}\"name");
        assert_eq!(t.name_bytes(file), b"bad_\xff\"name");
        let dir = find("dir_\u{FFFD}(");
        assert_eq!(t.name_bytes(dir), b"dir_\xc3\x28");
        assert_eq!(t.name_bytes(t.root), b"/r\xfe");
        assert_eq!(
            t.raw_name(find("ok")),
            None,
            "valid names need no side entry"
        );

        let mut out = Vec::new();
        export(&t, &mut out).unwrap();
        let has = |needle: &[u8]| out.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"\"name\":\"/r\xfe\""));
        assert!(has(b"\"name\":\"bad_\xff\\\"name\""));
        assert!(has(b"\"name\":\"dir_\xc3\x28\""));
        let again = import(&out[..]).unwrap();
        assert_eq!(again.name_bytes(file), b"bad_\xff\"name");
    }

    /// String escapes are decoded, including `\u` with surrogate pairs.
    #[test]
    fn import_decodes_string_escapes() {
        let json = r#"[1,2,{},[{"name":"/r"},
            {"name":"tab\there"},
            {"name":"quote\"back\\slash"},
            {"name":"café"},
            {"name":"😀.png"},
            {"name":"key-escaped"}]]"#;
        let t = import(json.as_bytes()).unwrap();
        let names: Vec<&str> = t.nodes[t.root]
            .children
            .iter()
            .map(|&c| t.nodes[c].name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "tab\there",
                "quote\"back\\slash",
                "café",
                "\u{1F600}.png",
                "key-escaped"
            ]
        );
    }

    /// Fields this version does not know are skipped, whatever their value looks like, so a
    /// dump with a higher minor version or extended information still loads.
    #[test]
    fn import_skips_unknown_fields() {
        let json = r#"[1, 3, {"progname":"ncdu","nested":{"a":[1,2,{"b":null}]},"n":-1.5e3},
            [ {"name":"/r","mode":16877,"mtime":-1.5,"uid":0,"future":[[],{},"]}\"",true,null]} ,
              {"extra":{"name":"not the name"},"name":"f","asize":10,"nlink":2,"x":false}
            ] ]
        "#;
        let t = import(json.as_bytes()).unwrap();
        assert_eq!(t.nodes[t.root].name, "/r");
        assert_eq!(t.nodes[t.root].items, 1);
        assert_eq!(t.nodes[t.root].apparent, 10);
        assert_eq!(t.nodes[t.nodes[t.root].children[0]].name, "f");
    }

    /// Anything that is not a dump is a clean error that says where it went wrong, never a
    /// panic and never a half-read tree.
    #[test]
    fn import_rejects_malformed_input() {
        let wrap = |child: &str| format!(r#"[1,2,{{}},[{{"name":"/r"}},{child}]]"#);
        let cases: Vec<(String, &str)> = vec![
            // Truncated at various points.
            ("".into(), "end of input"),
            ("[1,2,{}".into(), "end of input"),
            (r#"[1,2,{},[{"name":"/r"}"#.into(), "end of input"),
            (
                r#"[1,2,{},[{"name":"/r"},{"name":"f"#.into(),
                "end of input",
            ),
            (
                r#"[1,2,{},[{"name":"/r"},{"name":"f","asize":"#.into(),
                "end of input",
            ),
            (r#"[1,2,{},[{"name":"/r"}]"#.into(), "end of input"),
            // Wrong shapes.
            ("{}".into(), "`[`"),
            (r#"[1,2,{},{"name":"/r"}]"#.into(), "directory"),
            (r#"[1,2,{},[]]"#.into(), "`{`"),
            (r#"["1",2,{},[{"name":"/r"}]]"#.into(), "integer"),
            (r#"[1,2,[],[{"name":"/r"}]]"#.into(), "metadata"),
            (wrap("[]"), "`{`"),
            (wrap("7"), "entry"),
            (wrap("\"f\""), "entry"),
            (wrap("null"), "entry"),
            (wrap(r#"[["name"]]"#), "`{`"),
            // Wrong types and values in known fields.
            (wrap(r#"{"name":7}"#), "string"),
            (wrap(r#"{"name":"f","asize":"10"}"#), "integer"),
            (wrap(r#"{"name":"f","asize":-1}"#), "integer"),
            (wrap(r#"{"name":"f","asize":1.5}"#), "integer"),
            (
                wrap(r#"{"name":"f","dsize":99999999999999999999999}"#),
                "too large",
            ),
            (wrap(r#"{"name":"f","hlnkc":1}"#), "true or false"),
            (wrap(r#"{"name":"f","excluded":true}"#), "string"),
            (wrap(r#"{"asize":1}"#), "no name"),
            // Broken syntax.
            (wrap(r#"{"name":"f" "asize":1}"#), "`,` or `}`"),
            (wrap(r#"{"name":"f",}"#), "expected a string"),
            (wrap(r#"{"name":"f"} {"name":"g"}"#), "`,` or `]`"),
            (wrap(r#"{"name":"f"},"#), "entry"),
            (wrap(r#"{"name":"bad \q escape"}"#), "escape"),
            (wrap(r#"{"name":"f","x":[1,2}}"#), "mismatched"),
            (wrap("{\"name\":\"raw\nnewline\"}"), "control character"),
            // Trailing garbage.
            (r#"[1,2,{},[{"name":"/r"}]] x"#.into(), "after the end"),
            (r#"[1,2,{},[{"name":"/r"}],5]"#.into(), "`]`"),
            (r#"[1,2,{},[{"name":"/r"}]][]"#.into(), "after the end"),
        ];
        for (json, expect) in cases {
            let err = match import(json.as_bytes()) {
                Ok(_) => panic!("accepted: {json}"),
                Err(e) => e,
            };
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{json}");
            let msg = err.to_string();
            assert!(msg.contains("at byte "), "{json}: {msg}");
            assert!(msg.contains(expect), "{json}: got {msg:?}, want {expect:?}");
        }
    }

    /// The error names the position, so a damaged dump can be looked at.
    #[test]
    fn import_errors_carry_the_byte_offset() {
        let json = r#"[1,2,{},[{"name":"/r"},{"name":"f","asize":true}]]"#;
        let at = json.find("true").unwrap();
        let msg = import(json.as_bytes()).err().unwrap().to_string();
        assert!(msg.contains(&format!("at byte {at}:")), "{msg}");
    }

    /// The parser reads through a small buffer. Tokens that straddle a buffer boundary must
    /// come out whole; a reader that hands out one byte at a time puts every boundary there
    /// is to the test.
    #[test]
    fn import_reads_across_buffer_boundaries() {
        struct OneByte<'a>(&'a [u8]);
        impl Read for OneByte<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                match (self.0.split_first(), buf.first_mut()) {
                    (Some((&byte, rest)), Some(slot)) => {
                        *slot = byte;
                        self.0 = rest;
                        Ok(1)
                    }
                    _ => Ok(0),
                }
            }
        }
        let json = r#"[1,2,{"progname":"ncdu"},[{"name":"/r","asize":4096},
            [{"name":"café 😀","dsize":18446744073709551615},
             {"name":"f","asize":123456,"hlnkc":true,"read_error":false,"excluded":"otherfs"}]]]"#;
        let whole = import(json.as_bytes()).unwrap();
        let dripped = import(OneByte(json.as_bytes())).unwrap();
        assert_eq!(dripped.nodes.len(), whole.nodes.len());
        for (a, b) in whole.nodes.iter().zip(&dripped.nodes) {
            assert_eq!((&a.name, a.apparent, a.disk), (&b.name, b.apparent, b.disk));
            assert_eq!((a.hlink, a.excluded), (b.hlink, b.excluded));
        }
        assert_eq!(whole.nodes[1].name, "café \u{1F600}");
        assert_eq!(whole.nodes[1].disk, u64::MAX);
        assert_eq!(whole.nodes[2].excluded, Excluded::OtherFs);
    }

    /// Entry names are joined into real paths for delete/open/shell, so a dump must not be
    /// able to smuggle in `..`, separators or other names that resolve somewhere else
    /// (issue #1). Such dumps are rejected outright instead of being silently renamed.
    #[test]
    fn import_rejects_path_traversal_names() {
        let wrap = |child: &str| format!(r#"[1,2,{{}},[{{"name":"/r"}},{child}]]"#);
        for child in [
            r#"{"name":".."}"#,
            r#"{"name":"."}"#,
            r#"{"name":""}"#,
            r#"{"name":"a/b"}"#,
            r#"{"name":"/etc"}"#,
            r#"{"name":"a\u0000b"}"#,
            r#"[{"name":".."},{"name":"important","asize":1}]"#,
            r#"[{"name":"d"},[{"name":".."},{"name":"important"}]]"#,
        ] {
            let err = import(wrap(child).as_bytes()).err();
            assert_eq!(
                err.map(|e| e.kind()),
                Some(io::ErrorKind::InvalidData),
                "{child} must be rejected"
            );
        }
        // The root is a path, but still not one that climbs out or carries a NUL.
        for root in ["", "..", "/home/u/tmp/../..", "/r\\u0000x"] {
            let json = format!(r#"[1,2,{{}},[{{"name":"{root}"}}]]"#);
            assert!(
                import(json.as_bytes()).is_err(),
                "root {root:?} must be rejected"
            );
        }
    }

    /// Names that merely look unusual are ordinary file names and still import.
    #[test]
    fn import_accepts_unusual_but_safe_names() {
        let json = r#"[1,2,{},[{"name":"/r"},
            {"name":"..."},{"name":"..hidden"},{"name":"a b"},{"name":"back\\slash"},
            [{"name":".git"},{"name":"x."}]]]"#;
        let t = import(json.as_bytes()).unwrap();
        assert_eq!(t.nodes[t.root].items, 6);
        assert_eq!(t.path_of(t.nodes.len() - 1), "/r/.git/x.");
    }
}
