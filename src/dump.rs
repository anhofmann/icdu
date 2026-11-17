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

use crate::model::{Excluded, Ext, NodeIdx, NodeKind, Tree};

const MAJOR: u32 = 1;
const MINOR: u32 = 2;

// ---- Export ---------------------------------------------------------------------------------

/// Write the whole tree as a dump.
pub fn export<W: Write>(tree: &Tree, w: &mut W) -> io::Result<()> {
    let mut exporter = Exporter::new(tree, w)?;
    while !exporter.step(tree, || true)? {}
    Ok(())
}

/// How many entries an [`Exporter`] writes between two looks at the clock.
const EXPORT_CHECK_EVERY: u64 = 1024;

/// A dump that is written a piece at a time, so that writing a large one does not freeze the
/// UI: the event loop calls [`Exporter::step`] for a few milliseconds per frame.
///
/// The directories being written are kept on a stack on the heap, each with the position of
/// its next child, so a very deep tree cannot exhaust the call stack either.
///
/// The tree may grow between steps (a scan is still delivering): entries that arrive in a
/// directory that is not finished yet are written too. Entries must not be removed while an
/// export runs, because that shifts the positions; the app refuses deletes and rescans for
/// that time.
pub struct Exporter<W: Write> {
    w: W,
    open: Vec<(NodeIdx, usize)>,
    /// Entries written so far.
    pub written: u64,
    done: bool,
}

impl<W: Write> Exporter<W> {
    /// Start a dump of `tree`: writes the header and the root's own entry.
    pub fn new(tree: &Tree, mut w: W) -> io::Result<Self> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        write!(
            w,
            "[{MAJOR},{MINOR},{{\"progname\":\"icdu\",\"progver\":\"{}\",\"timestamp\":{ts}}},",
            env!("CARGO_PKG_VERSION")
        )?;
        w.write_all(b"[")?;
        write_item(tree, tree.root, &mut w)?;
        Ok(Exporter {
            w,
            open: vec![(tree.root, 0)],
            written: 1,
            done: false,
        })
    }

    /// Write on until the dump is complete or `keep_going` says no (it is asked every
    /// [`EXPORT_CHECK_EVERY`] entries). Returns whether the dump is complete; it is flushed
    /// then.
    pub fn step(&mut self, tree: &Tree, mut keep_going: impl FnMut() -> bool) -> io::Result<bool> {
        if self.done {
            return Ok(true);
        }
        let mut until_check = EXPORT_CHECK_EVERY;
        while let Some((dir, next)) = self.open.last_mut() {
            let Some(&child) = tree.nodes[*dir].children.get(*next) else {
                self.w.write_all(b"]")?;
                self.open.pop();
                continue;
            };
            *next += 1;
            self.w.write_all(b",")?;
            if tree.nodes[child].is_dir() {
                self.w.write_all(b"[")?;
                write_item(tree, child, &mut self.w)?;
                self.open.push((child, 0));
            } else {
                write_item(tree, child, &mut self.w)?;
            }
            self.written += 1;
            until_check -= 1;
            if until_check == 0 {
                if !keep_going() {
                    return Ok(false);
                }
                until_check = EXPORT_CHECK_EVERY;
            }
        }
        self.w.write_all(b"]\n")?;
        self.w.flush()?;
        self.done = true;
        Ok(true)
    }
}

/// Write the info object of one entry: the fields ncdu writes, in ncdu's order, and like
/// ncdu only those that say something. A size of zero is left out, the inode is there for
/// hard links only, and the device only where it differs from the parent's.
fn write_item<W: Write>(tree: &Tree, idx: NodeIdx, w: &mut W) -> io::Result<()> {
    let n = &tree.nodes[idx];
    write!(w, "{{\"name\":")?;
    write_json_string(tree.name_bytes(idx), w)?;
    // An entry that was left out has no size in a dump, as in ncdu, which does not even
    // look at it. A reader that added one to its totals would disagree with the scan.
    let (apparent, disk) = match n.excluded {
        Excluded::No => (tree.own_apparent(idx), tree.own_disk(idx)),
        _ => (0, 0),
    };
    if apparent > 0 {
        write!(w, ",\"asize\":{apparent}")?;
    }
    if disk > 0 {
        write!(w, ",\"dsize\":{disk}")?;
    }
    // A reader takes a missing `dev` to be the parent's; the root has no parent.
    if n.parent
        .is_none_or(|parent| tree.nodes[parent].dev != n.dev)
    {
        write!(w, ",\"dev\":{}", n.dev)?;
    }
    if let Some(ext) = tree.ext(idx) {
        if let Some(uid) = ext.uid() {
            write!(w, ",\"uid\":{uid}")?;
        }
        if let Some(gid) = ext.gid() {
            write!(w, ",\"gid\":{gid}")?;
        }
        if let Some(mode) = ext.mode() {
            write!(w, ",\"mode\":{mode}")?;
        }
        if let Some(mtime) = ext.mtime() {
            write!(w, ",\"mtime\":{mtime}")?;
        }
    }
    if n.hlink {
        write!(w, ",\"ino\":{},\"hlnkc\":true", n.ino)?;
        if let Some(nlink) = tree.nlink(idx) {
            write!(w, ",\"nlink\":{nlink}")?;
        }
    }
    // A directory whose listing has not arrived yet (a dump taken while the scan is still
    // running) is written as a read error too. The format has no word for "not read yet", and
    // this one makes every reader show the directory as incomplete instead of as empty.
    let read_error = n.read_error || tree.is_unread(idx);
    if read_error {
        write!(w, ",\"read_error\":true")?;
    }
    // "Not a regular file" is for symlinks and special files. An entry that could not be
    // examined, or was left out, is of unknown type, and ncdu does not flag those.
    let special = matches!(n.kind, NodeKind::Link | NodeKind::Other);
    if special && !read_error && n.excluded == Excluded::No {
        write!(w, ",\"notreg\":true")?;
    }
    match n.excluded {
        // The mtime filters are icdu's own. The format has no value for them, and a reader
        // takes an unknown value for a pattern anyway.
        Excluded::Pattern | Excluded::Age => write!(w, ",\"excluded\":\"pattern\"")?,
        Excluded::OtherFs => write!(w, ",\"excluded\":\"otherfs\"")?,
        Excluded::Kernfs => write!(w, ",\"excluded\":\"kernfs\"")?,
        Excluded::FrmLink => write!(w, ",\"excluded\":\"frmlink\"")?,
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
#[cfg(test)]
pub fn import<R: Read>(r: R) -> io::Result<Tree> {
    import_with(r, false)
}

/// Read a dump. With `extended` (`-e`) the tree also keeps owner, group, mode and
/// modification time of the entries that have them in the dump; without it they are passed
/// over, as ncdu does, and cost no memory.
pub fn import_with<R: Read>(r: R, extended: bool) -> io::Result<Tree> {
    let mut p = Parser::new(r);
    p.refuse_other_formats()?;
    p.expect(b'[')?;
    // Format version. Only major version 1 is understood. A higher minor version still
    // loads: what it adds is in fields, and unknown fields are skipped.
    let major = p.uint()?;
    if major != MAJOR as u64 {
        return Err(p.error(format_args!(
            "incompatible major format version {major} (this is version {MAJOR})"
        )));
    }
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
    let root_dev = item.dev.unwrap_or(0);
    let mut tree = Tree::new_imported(name, item.apparent, item.disk, root_dev, item.ino);
    if let Some(raw) = raw {
        tree.set_raw_name(tree.root, raw);
    }
    if extended {
        tree.set_extended();
        tree.set_ext(tree.root, item.ext);
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
                // The format writes `dev` only where it changes: without one, an entry is on
                // the device of its parent.
                let dev = item.dev.unwrap_or(tree.nodes[parent].dev);
                // Re-derive hard-link sharing so imported totals dedupe like a live scan.
                // As there, an excluded link is never the one that counts, and so does not
                // make a later link a duplicate.
                let shared = !is_dir
                    && item.hlink
                    && item.excluded == Excluded::No
                    && !seen.insert((dev, item.ino));
                let (name, raw) = display_name(&item.name);
                let idx = tree.import_child(
                    parent,
                    name,
                    kind,
                    item.apparent,
                    item.disk,
                    dev,
                    item.ino,
                    item.hlink,
                    shared,
                    item.excluded,
                    item.read_error,
                );
                if let Some(raw) = raw {
                    tree.set_raw_name(idx, raw);
                }
                if let Some(nlink) = item.nlink {
                    tree.set_nlink(idx, u32::try_from(nlink).unwrap_or(u32::MAX));
                }
                if extended && item.ext.has != 0 {
                    tree.set_ext(idx, item.ext);
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
    /// Absent where it is the parent's.
    dev: Option<u64>,
    ino: u64,
    /// The number of links, written for entries that have more than one.
    nlink: Option<u64>,
    hlink: bool,
    notreg: bool,
    read_error: bool,
    excluded: Excluded,
    /// Owner, group, mode and modification time, as far as the dump has them.
    ext: Ext,
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

const WANT_UINT: &str = "expected a non-negative integer";

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

    /// Before anything is parsed: say so if the input is one of the other things ncdu can
    /// write since 2.6, a zstd-compressed dump (`-c`) or a binary export (`-O`). Neither is
    /// supported, and "expected `[`" would be no help.
    fn refuse_other_formats(&mut self) -> io::Result<()> {
        const ZSTD: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
        const BINARY: &[u8] = b"\xbfncduEX1";
        // Gather the first few bytes, however slowly the reader hands them out.
        while self.len < BINARY.len() {
            match self.src.read(&mut self.buf[self.len..]) {
                Ok(0) => break,
                Ok(n) => self.len += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let start = &self.buf[..self.len];
        if start.starts_with(ZSTD) {
            return Err(self.error(
                "this is a zstd-compressed ncdu export, which icdu cannot read; \
                 decompress it first: zstd -dc FILE | icdu -f -",
            ));
        }
        // Not something ncdu writes itself, but what its man page suggests for saving space
        // (`ncdu -o- | gzip >export.gz`), and easy to pass in by mistake.
        if start.starts_with(&[0x1f, 0x8b]) {
            return Err(self.error(
                "this is a gzip-compressed file; decompress it first: \
                 zcat FILE | icdu -f -",
            ));
        }
        if start.starts_with(BINARY) {
            return Err(self.error(
                "this is a binary export of ncdu (-O), which icdu cannot read; \
                 convert it first: ncdu -f FILE -o - | icdu -f -",
            ));
        }
        Ok(())
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
        let value = self.digits()?;
        if matches!(self.peek()?, Some(b'.' | b'e' | b'E')) {
            return Err(self.error(WANT_UINT));
        }
        Ok(value)
    }

    /// The digits of a non-negative integer.
    fn digits(&mut self) -> io::Result<u64> {
        if !self.peek_token()?.is_ascii_digit() {
            return Err(self.error(WANT_UINT));
        }
        let mut value = 0u64;
        while let Some(digit @ b'0'..=b'9') = self.peek()? {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(digit - b'0')))
                .ok_or_else(|| self.error("number too large"))?;
            self.bump();
        }
        Ok(value)
    }

    /// A time in seconds. ncdu takes a fraction here and drops it, so a dump of a tool that
    /// writes one loads.
    fn seconds(&mut self) -> io::Result<u64> {
        let value = self.digits()?;
        if self.peek()? == Some(b'.') {
            self.bump();
            while let Some(b'0'..=b'9') = self.peek()? {
                self.bump();
            }
        }
        if matches!(self.peek()?, Some(b'e' | b'E')) {
            return Err(self.error(WANT_UINT));
        }
        Ok(value)
    }

    /// A number that has to fit the field it is for.
    fn uint_up_to(&mut self, max: u64) -> io::Result<u64> {
        match self.uint()? {
            value if value <= max => Ok(value),
            _ => Err(self.error("number too large")),
        }
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
            b"dev" => self.uint().map(|v| item.dev = Some(v)),
            b"nlink" => self.uint().map(|v| item.nlink = Some(v)),
            b"ino" => self.uint().map(|v| item.ino = v),
            // ncdu's extended information, with the ranges ncdu gives the fields.
            b"uid" => self.uint_up_to(u64::from(u32::MAX)).map(|v| {
                item.ext.uid = v as u32;
                item.ext.has |= Ext::UID;
            }),
            b"gid" => self.uint_up_to(u64::from(u32::MAX)).map(|v| {
                item.ext.gid = v as u32;
                item.ext.has |= Ext::GID;
            }),
            b"mode" => self.uint_up_to(u64::from(u16::MAX)).map(|v| {
                item.ext.mode = v as u16;
                item.ext.has |= Ext::MODE;
            }),
            b"mtime" => self.seconds().map(|v| {
                item.ext.mtime = v;
                item.ext.has |= Ext::MTIME;
            }),
            b"hlnkc" => self.boolean().map(|v| item.hlink = v),
            b"notreg" => self.boolean().map(|v| item.notreg = v),
            b"read_error" => self.boolean().map(|v| item.read_error = v),
            b"excluded" => {
                let mut reason = Vec::new();
                self.string(&mut reason)?;
                item.excluded = match reason.as_slice() {
                    // `othfs` is what ncdu wrote before 1.21 and 2.5.
                    b"otherfs" | b"othfs" => Excluded::OtherFs,
                    b"kernfs" => Excluded::Kernfs,
                    b"frmlink" => Excluded::FrmLink,
                    b"age" => Excluded::Age,
                    // `pattern`, and any reason a later version may add.
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

    /// The body of a dump: everything after the metadata object, whose timestamp differs
    /// from one export to the next.
    fn body(dump: &[u8]) -> &[u8] {
        let meta_end = dump.iter().position(|&b| b == b'}').unwrap();
        &dump[meta_end..]
    }

    /// Written in many small steps, a dump is byte for byte what it is written in one go.
    #[test]
    fn export_in_steps_writes_the_same_dump() {
        let mut t = Tree::new_imported("/r".into(), 4096, 4096, 1, 1);
        let mut dirs = vec![t.root];
        for i in 0..5_000u64 {
            let parent = dirs[(i as usize * 7) % dirs.len()];
            let kind = if i % 9 == 0 {
                NodeKind::Dir
            } else {
                NodeKind::File
            };
            let idx = t.import_child(
                parent,
                format!("entry-{i}").into(),
                kind,
                i,
                i * 2,
                1,
                i + 2,
                false,
                false,
                Excluded::No,
                false,
            );
            if kind == NodeKind::Dir {
                dirs.push(idx);
            }
        }
        let mut whole = Vec::new();
        export(&t, &mut whole).unwrap();

        let mut pieces = Vec::new();
        let mut exporter = Exporter::new(&t, &mut pieces).unwrap();
        let mut steps = 0;
        while !exporter.step(&t, || false).unwrap() {
            steps += 1;
            assert!(steps < 100, "the export does not advance");
        }
        assert_eq!(steps, 4, "5,001 entries, {EXPORT_CHECK_EVERY} per step");
        assert_eq!(exporter.written, 5_001);
        assert!(exporter.step(&t, || false).unwrap(), "done stays done");
        drop(exporter);
        assert_eq!(body(&pieces), body(&whole));
        assert_eq!(import(&pieces[..]).unwrap().total_files, t.total_files);
    }

    /// Entries that arrive while an export is under way are written if their directory has
    /// not been finished yet, and the dump stays well-formed either way.
    #[test]
    fn export_in_steps_copes_with_a_growing_tree() {
        let mut t = Tree::new_imported("/r".into(), 0, 0, 1, 1);
        let add = |t: &mut Tree, parent, name: String, kind| {
            t.import_child(
                parent,
                name.into(),
                kind,
                1,
                1,
                1,
                2,
                false,
                false,
                Excluded::No,
                false,
            )
        };
        let first = add(&mut t, 0, "first".into(), NodeKind::Dir);
        for i in 0..2_000 {
            add(&mut t, first, format!("f{i}"), NodeKind::File);
        }
        let last = add(&mut t, 0, "last".into(), NodeKind::Dir);

        let mut out = Vec::new();
        let mut exporter = Exporter::new(&t, &mut out).unwrap();
        assert!(
            !exporter.step(&t, || false).unwrap(),
            "stopped inside `first`"
        );
        // The scan delivers more: into the directory being written, into one that is still
        // to come, and a new one at the top.
        add(&mut t, first, "late-in-first".into(), NodeKind::File);
        add(&mut t, last, "late-in-last".into(), NodeKind::File);
        add(&mut t, 0, "late-at-top".into(), NodeKind::Dir);
        while !exporter.step(&t, || false).unwrap() {}
        drop(exporter);

        let again = import(&out[..]).unwrap();
        let has = |name: &str| again.nodes.iter().any(|n| n.name == name);
        assert!(has("late-in-first") && has("late-in-last") && has("late-at-top"));
        assert_eq!(again.nodes.len(), t.nodes.len());
    }

    /// A dump written by ncdu 1.18 for a small tree with one of everything: nested
    /// directories, an empty file, two links to one inode, a symlink, a fifo, a directory
    /// excluded by pattern and one that could not be read. (The path of the scanned directory
    /// and the timestamp were shortened by hand; everything else is as ncdu wrote it.)
    const NCDU_1_18: &str = include_str!("../fixtures/ncdu-1.18.json");

    fn named<'a>(t: &'a Tree, name: &str) -> (NodeIdx, &'a crate::model::Node) {
        let idx = t
            .nodes
            .iter()
            .position(|n| n.name == name)
            .unwrap_or_else(|| panic!("no entry called {name}"));
        (idx, &t.nodes[idx])
    }

    /// After importing ncdu's dump, the totals and every field are what ncdu recorded.
    #[test]
    fn import_reads_an_ncdu_dump_completely() {
        let t = import(NCDU_1_18.as_bytes()).unwrap();
        let root = &t.nodes[t.root];
        assert_eq!(root.name, "/fixture");
        assert_eq!(root.items, 12);
        // Every size once: the second link to the inode and the excluded directory add nothing.
        assert_eq!(root.apparent, 4096 * 5 + 5000 + 2 + 18 + 6);
        assert_eq!(root.disk, 4096 * 5 + 8192 + 4096 + 4096);
        // ncdu writes an excluded directory as a plain entry, so it counts as a file here.
        assert_eq!((t.total_dirs, t.total_files), (5, 8));

        // `dev` is written once, at the root, and holds for everything below it.
        for name in [
            "links", "hard.txt", "docs", "deep", "data.bin", "empty", "locked",
        ] {
            assert_eq!(named(&t, name).1.dev, 64513, "{name}");
        }
        // The excluded directory carries a `dev` of its own in the dump.
        let (_, build) = named(&t, "build");
        assert_eq!((build.dev, build.excluded), (0, Excluded::Pattern));

        let (hard, first) = named(&t, "hard.txt");
        let (again, second) = named(&t, "hard-again.txt");
        assert_eq!((first.ino, second.ino), (3297454, 3297454));
        assert!(first.hlink && second.hlink);
        assert!(
            !first.shared && second.shared,
            "the first link in the dump counts"
        );
        assert_eq!((t.nlink(hard), t.nlink(again)), (Some(2), Some(2)));
        assert_eq!(t.nlink(named(&t, "readme.txt").0), None);

        assert!(named(&t, "locked").1.read_error);
        assert!(!named(&t, "docs").1.read_error);
        for (name, apparent) in [("fifo", 0), ("symlink", 18)] {
            let (idx, n) = named(&t, name);
            assert!(
                matches!(n.kind, NodeKind::Other),
                "{name} is not a regular file"
            );
            assert_eq!(t.own_apparent(idx), apparent);
        }
        let (data, _) = named(&t, "data.bin");
        assert_eq!((t.own_apparent(data), t.own_disk(data)), (5000, 8192));
        assert_eq!(t.path_of(data), "/fixture/docs/deep/data.bin");
    }

    /// A dump without its header (program name, version, timestamp) and without the line
    /// breaks ncdu puts between entries.
    fn entries(dump: &str) -> String {
        let meta_end = dump.find('}').unwrap();
        dump[meta_end..].replace('\n', "")
    }

    /// Read ncdu's dump and write it back: every entry comes out with the same fields, the
    /// same values and in the same order as ncdu wrote it. So what icdu writes is what ncdu
    /// would have written, and nothing ncdu recorded is lost on the way through.
    #[test]
    fn export_writes_what_ncdu_writes() {
        let t = import(NCDU_1_18.as_bytes()).unwrap();
        let mut out = Vec::new();
        export(&t, &mut out).unwrap();
        let ours = String::from_utf8(out).unwrap();
        assert_eq!(entries(&ours), entries(NCDU_1_18));
    }

    /// The fields that are only written when they say something.
    #[test]
    fn export_leaves_out_what_goes_without_saying() {
        let json = r#"[1,2,{},[{"name":"/r","asize":4096,"dsize":4096,"dev":7},
            {"name":"empty"},
            {"name":"plain","asize":5,"dsize":512,"ino":100},
            {"name":"linked","asize":5,"dsize":512,"ino":101,"hlnkc":true,"nlink":2},
            [{"name":"mnt","dev":8},{"name":"inside","asize":1}],
            {"name":"aged","asize":9,"excluded":"age"}]]"#;
        let t = import(json.as_bytes()).unwrap();
        let mut out = Vec::new();
        export(&t, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        for written in [
            // The root always says which device it is on.
            r#"[{"name":"/r","asize":4096,"dsize":4096,"dev":7}"#,
            // No sizes of zero, no inode without a hard link, no device like the parent's.
            r#"{"name":"empty"}"#,
            r#"{"name":"plain","asize":5,"dsize":512}"#,
            r#"{"name":"linked","asize":5,"dsize":512,"ino":101,"hlnkc":true,"nlink":2}"#,
            // A device that differs is written, and holds for what is below.
            r#"[{"name":"mnt","dev":8},{"name":"inside","asize":1}]"#,
            // icdu's mtime filter is not a value of the format, and what was left out has no
            // size.
            r#"{"name":"aged","excluded":"pattern"}"#,
        ] {
            assert!(text.contains(written), "{written}\nnot in\n{text}");
        }
        assert!(!text.contains("\"age\""));
    }

    /// What `ncdu -e` wrote for a tree with several owners, modes (setuid, sticky, a fifo, a
    /// symlink) and times, a hard link and an excluded directory. (The path of the scanned
    /// directory was shortened by hand.)
    const NCDU_1_18_EXTENDED: &str = include_str!("../fixtures/ncdu-1.18-extended.json");

    /// An extended dump, read with `-e` and written back: every entry has the fields ncdu
    /// wrote, with the same values and in the same order.
    #[test]
    fn extended_dump_passes_through_unchanged() {
        let t = import_with(NCDU_1_18_EXTENDED.as_bytes(), true).unwrap();
        let mut out = Vec::new();
        export(&t, &mut out).unwrap();
        let ours = String::from_utf8(out).unwrap();
        assert_eq!(entries(&ours), entries(NCDU_1_18_EXTENDED));

        let ext = |name: &str| t.ext(named(&t, name).0);
        let script = ext("script.sh").unwrap();
        assert_eq!(
            (script.uid, script.gid, script.mode, script.mtime),
            (1000, 100, 0o100750, 1_709_208_000)
        );
        assert_eq!(ext("setuid-tool").unwrap().mode, 0o104755);
        assert_eq!(ext("shared-tmp").unwrap().mode, 0o041777);
        assert_eq!(ext("symlink").unwrap().mode, 0o120777);
        assert_eq!(ext("/fixture").unwrap().mtime, 1_514_764_800);
        // ncdu does not look at what a pattern excludes, so there is nothing about it.
        assert_eq!(ext("build"), None);
        // A directory's latest time is the latest below it.
        assert_eq!(t.latest_mtime(named(&t, "docs").0), Some(1_709_208_000));
        assert_eq!(t.latest_mtime(named(&t, "links").0), Some(1_640_995_200));
        assert_eq!(t.latest_mtime(t.root), Some(1_709_208_000));
    }

    /// Without `-e` the fields are passed over, as in ncdu: the tree does not keep them, and
    /// they are not written back. Everything else is the same tree.
    #[test]
    fn extended_fields_need_the_flag() {
        let plain = import(NCDU_1_18_EXTENDED.as_bytes()).unwrap();
        assert!(!plain.is_extended());
        assert_eq!(plain.ext(plain.root), None);
        let mut out = Vec::new();
        export(&plain, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        for field in ["\"uid\"", "\"gid\"", "\"mode\"", "\"mtime\""] {
            assert!(!text.contains(field), "{field} in {text}");
        }
        let extended = import_with(NCDU_1_18_EXTENDED.as_bytes(), true).unwrap();
        assert_eq!(plain.nodes.len(), extended.nodes.len());
        assert_eq!(
            plain.nodes[plain.root].apparent,
            extended.nodes[extended.root].apparent
        );
    }

    /// The values of the four fields, read the way ncdu 1.18 reads them (probed with dumps
    /// like these), with one exception: ncdu 1.18 stops at a uid of 2^31, ncdu 2 and the
    /// kernel at 2^32.
    #[test]
    fn extended_fields_are_read_like_ncdu_reads_them() {
        let dump =
            |fields: &str| format!(r#"[1,2,{{}},[{{"name":"/r"}},{{"name":"f",{fields}}}]]"#);
        let read = |fields: &str| {
            import_with(dump(fields).as_bytes(), true).map(|t| (t.ext(1), written(&t)))
        };
        // A fraction of a second is dropped.
        let (ext, text) = read(r#""mtime":17.999"#).unwrap();
        assert_eq!(ext.unwrap().mtime(), Some(17));
        assert!(text.contains(r#"{"name":"f","mtime":17}"#), "{text}");
        let (ext, _) = read(r#""mtime":18446744073709551615"#).unwrap();
        assert_eq!(ext.unwrap().mtime(), Some(u64::MAX));
        // A dump may have some of the fields. Those are kept, and no others are made up.
        let (ext, text) = read(r#""uid":5"#).unwrap();
        let ext = ext.unwrap();
        assert_eq!(
            (ext.uid(), ext.gid(), ext.mode(), ext.mtime()),
            (Some(5), None, None, None)
        );
        assert!(text.contains(r#"{"name":"f","uid":5}"#), "{text}");
        let (ext, _) = read(r#""uid":4294967295,"gid":0,"mode":65535"#).unwrap();
        let ext = ext.unwrap();
        assert_eq!(
            (ext.uid(), ext.gid(), ext.mode(), ext.mtime()),
            (Some(u32::MAX), Some(0), Some(u16::MAX), None)
        );
        assert_eq!(read(r#""asize":1"#).unwrap().0, None);
        // What does not fit, or is no number, is an error, with `-e` and without.
        for bad in [
            r#""mtime":-1"#,
            r#""mtime":1e3"#,
            r#""mtime":"yesterday""#,
            r#""uid":4294967296"#,
            r#""gid":-1"#,
            r#""gid":1.5"#,
            r#""mode":65536"#,
            r#""mode":"x""#,
        ] {
            assert!(read(bad).is_err(), "{bad}");
            assert!(import(dump(bad).as_bytes()).is_err(), "{bad} without -e");
        }
    }

    fn written(t: &Tree) -> String {
        let mut out = Vec::new();
        export(t, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// An entry without `dev` is on the device of its parent. Hard links are told apart by
    /// device and inode, so two files with the same inode number on different devices are two
    /// files; reading the missing `dev` as 0 merged them.
    #[test]
    fn import_inherits_dev_from_the_parent() {
        let json = r#"[1,2,{},[{"name":"/r","dev":10},
            [{"name":"a"},{"name":"f1","asize":100,"ino":7,"hlnkc":true}],
            [{"name":"mnt","dev":20},
                {"name":"f2","asize":100,"ino":7,"hlnkc":true},
                [{"name":"deeper"},{"name":"f3","asize":100,"ino":7,"hlnkc":true}]]]]"#;
        let t = import(json.as_bytes()).unwrap();
        let dev = |name| named(&t, name).1.dev;
        assert_eq!((dev("a"), dev("f1")), (10, 10));
        assert_eq!(
            (dev("mnt"), dev("f2"), dev("deeper"), dev("f3")),
            (20, 20, 20, 20)
        );
        assert!(!named(&t, "f1").1.shared);
        assert!(!named(&t, "f2").1.shared, "another device: another file");
        assert!(named(&t, "f3").1.shared, "the same device and inode as f2");
        assert_eq!(t.nodes[t.root].apparent, 200);
    }

    /// Only format version 1 can be read. A higher minor version is fine: what it adds is
    /// in fields that are skipped.
    #[test]
    fn import_checks_the_major_version() {
        for (dump, ok) in [
            (r#"[1,0,{},[{"name":"/r"}]]"#, true),
            (r#"[1,2,{},[{"name":"/r"}]]"#, true),
            (r#"[1,99,{},[{"name":"/r"}]]"#, true),
            (r#"[2,0,{},[{"name":"/r"}]]"#, false),
            (r#"[0,2,{},[{"name":"/r"}]]"#, false),
        ] {
            match import(dump.as_bytes()) {
                Ok(_) => assert!(ok, "accepted: {dump}"),
                Err(e) => {
                    assert!(!ok, "{dump}: {e}");
                    assert!(e.to_string().contains("major format version"), "{e}");
                }
            }
        }
    }

    /// Why an entry was left out survives an import and goes back out unchanged, in the
    /// spelling of today. A reason this version does not know is treated as a pattern.
    #[test]
    fn import_keeps_exclusion_reasons() {
        let json = r#"[1,2,{},[{"name":"/r"},
            {"name":"p","excluded":"pattern"},
            {"name":"o","excluded":"otherfs"},
            {"name":"old","excluded":"othfs"},
            {"name":"k","excluded":"kernfs"},
            {"name":"f","excluded":"frmlink"},
            {"name":"future","excluded":"something new"}]]"#;
        let t = import(json.as_bytes()).unwrap();
        let reason = |name| named(&t, name).1.excluded;
        assert_eq!(reason("p"), Excluded::Pattern);
        assert_eq!(reason("o"), Excluded::OtherFs);
        assert_eq!(
            reason("old"),
            Excluded::OtherFs,
            "the spelling before ncdu 1.21"
        );
        assert_eq!(reason("k"), Excluded::Kernfs);
        assert_eq!(reason("f"), Excluded::FrmLink);
        assert_eq!(reason("future"), Excluded::Pattern);

        let mut out = Vec::new();
        export(&t, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        for written in [
            r#"{"name":"o","excluded":"otherfs"}"#,
            r#"{"name":"old","excluded":"otherfs"}"#,
            r#"{"name":"k","excluded":"kernfs"}"#,
            r#"{"name":"f","excluded":"frmlink"}"#,
        ] {
            assert!(text.contains(written), "{written} not in {text}");
        }
    }

    /// An excluded link to an inode is not the one that counts. If it comes first in the
    /// dump, the next link still has to be counted, or the inode is counted nowhere.
    #[test]
    fn import_does_not_count_an_excluded_link_as_seen() {
        let json = r#"[1,2,{},[{"name":"/r","dev":1},
            {"name":"skipped","asize":500,"ino":9,"hlnkc":true,"excluded":"pattern"},
            {"name":"kept","asize":500,"ino":9,"hlnkc":true},
            {"name":"again","asize":500,"ino":9,"hlnkc":true}]]"#;
        let t = import(json.as_bytes()).unwrap();
        assert!(
            !named(&t, "kept").1.shared,
            "the first link that is not excluded counts"
        );
        assert!(named(&t, "again").1.shared);
        assert_eq!(t.nodes[t.root].apparent, 500);
    }

    /// `nlink` is kept for the entries that have more than one link.
    #[test]
    fn import_keeps_link_counts() {
        let json = r#"[1,2,{},[{"name":"/r"},
            {"name":"three","ino":5,"hlnkc":true,"nlink":3},
            {"name":"one","nlink":1},
            {"name":"none"}]]"#;
        let t = import(json.as_bytes()).unwrap();
        assert_eq!(t.nlink(named(&t, "three").0), Some(3));
        assert_eq!(t.nlink(named(&t, "one").0), None);
        assert_eq!(t.nlink(named(&t, "none").0), None);
    }

    /// ncdu 2.6 and later can write zstd-compressed dumps and a binary export. Neither can
    /// be read here, and the error says what the file is instead of complaining about JSON.
    #[test]
    fn import_names_compressed_and_binary_exports() {
        let zstd = [0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x58, 0x45, 0x00];
        let e = import(&zstd[..]).err().unwrap().to_string();
        assert!(e.contains("zstd-compressed"), "{e}");
        assert!(e.contains("zstd -dc"), "{e}");

        let gzip = [0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03];
        let e = import(&gzip[..]).err().unwrap().to_string();
        assert!(e.contains("gzip-compressed") && e.contains("zcat"), "{e}");

        let binary = b"\xbfncduEX1\x00\x00\x00\x08";
        let e = import(&binary[..]).err().unwrap().to_string();
        assert!(e.contains("binary export"), "{e}");
        assert!(e.contains("ncdu -f"), "{e}");

        // Recognised however the bytes arrive, and nothing else is taken for either.
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
        let e = import(OneByte(&zstd)).err().unwrap().to_string();
        assert!(e.contains("zstd-compressed"), "{e}");
        let e = import(&b"(not a dump)"[..]).err().unwrap().to_string();
        assert!(e.contains("expected `[`"), "{e}");
        assert!(import(&b""[..]).is_err());
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
    /// dump with a higher minor version still loads.
    #[test]
    fn import_skips_unknown_fields() {
        let json = r#"[1, 3, {"progname":"ncdu","nested":{"a":[1,2,{"b":null}]},"n":-1.5e3},
            [ {"name":"/r","kind":16877,"when":-1.5,"who":0,"future":[[],{},"]}\"",true,null]} ,
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
