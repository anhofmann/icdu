//! ncdu-compatible JSON import/export.
//!
//! Format (ncdu export version 1): a top-level array
//!     `[major, minor, metadata, root]`
//! where each directory is itself an array `[info, child, child, ...]` beginning with its own
//! info object, and files are plain info objects. Sizes are stored *per item* (`asize`/`dsize`
//! are the entry's own size); totals are recomputed on load.
//!
//! Export is written by hand (streaming, no intermediate `Value`) so it stays cheap on huge
//! trees. Import uses `serde_json` for robustness against the variety of dumps in the wild.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

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
        "[{MAJOR},{MINOR},{{\"progname\":\"rcdu\",\"progver\":\"{}\",\"timestamp\":{ts}}},",
        env!("CARGO_PKG_VERSION")
    )?;
    write_node(tree, tree.root, w)?;
    writeln!(w, "]")
}

fn write_node<W: Write>(tree: &Tree, idx: NodeIdx, w: &mut W) -> io::Result<()> {
    let n = &tree.nodes[idx];
    if n.is_dir() {
        w.write_all(b"[")?;
        write_item(tree, idx, w)?;
        for &c in &n.children {
            w.write_all(b",")?;
            write_node(tree, c, w)?;
        }
        w.write_all(b"]")
    } else {
        write_item(tree, idx, w)
    }
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

pub fn import<R: Read>(r: R) -> io::Result<Tree> {
    let v: Value = serde_json::from_reader(r)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}")))?;
    let top = v
        .as_array()
        .ok_or_else(|| bad("top level must be an array [major, minor, meta, root]"))?;
    let root_val = top
        .get(3)
        .ok_or_else(|| bad("missing root element (index 3)"))?;

    // The root must be a directory array: [info, children...].
    let arr = root_val
        .as_array()
        .ok_or_else(|| bad("root must be a directory array"))?;
    let info = arr.first().ok_or_else(|| bad("empty root directory"))?;

    let item = parse_item(info);
    check_root_name(&item.name)?;
    let mut tree = Tree::new_imported(
        item.name.into(),
        item.apparent,
        item.disk,
        item.dev,
        item.ino,
    );
    let root = tree.root;

    // Dedup hard links during import too, so totals match a live scan.
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    for child in &arr[1..] {
        add_value(&mut tree, root, child, &mut seen)?;
    }
    Ok(tree)
}

struct Item {
    name: String,
    apparent: u64,
    disk: u64,
    dev: u64,
    ino: u64,
    hlink: bool,
    notreg: bool,
    read_error: bool,
    excluded: Excluded,
}

fn parse_item(v: &Value) -> Item {
    let g = |k: &str| v.get(k);
    Item {
        name: g("name").and_then(Value::as_str).unwrap_or("?").to_string(),
        apparent: g("asize").and_then(Value::as_u64).unwrap_or(0),
        disk: g("dsize").and_then(Value::as_u64).unwrap_or(0),
        dev: g("dev").and_then(Value::as_u64).unwrap_or(0),
        ino: g("ino").and_then(Value::as_u64).unwrap_or(0),
        hlink: g("hlnkc").and_then(Value::as_bool).unwrap_or(false),
        notreg: g("notreg").and_then(Value::as_bool).unwrap_or(false),
        read_error: g("read_error").and_then(Value::as_bool).unwrap_or(false),
        excluded: match g("excluded").and_then(Value::as_str) {
            Some("otherfs") => Excluded::OtherFs,
            Some("age") => Excluded::Age,
            Some(_) => Excluded::Pattern,
            None => Excluded::No,
        },
    }
}

fn add_value(
    tree: &mut Tree,
    parent: NodeIdx,
    v: &Value,
    seen: &mut HashSet<(u64, u64)>,
) -> io::Result<()> {
    if let Some(arr) = v.as_array() {
        // Directory: [info, children...].
        let Some(info) = arr.first() else {
            return Ok(());
        };
        let item = parse_item(info);
        check_name(&item.name)?;
        let idx = tree.import_child(
            parent,
            item.name.into(),
            NodeKind::Dir,
            item.apparent,
            item.disk,
            item.dev,
            item.ino,
            item.hlink,
            false,
            item.excluded,
            item.read_error,
        );
        for child in &arr[1..] {
            add_value(tree, idx, child, seen)?;
        }
    } else {
        let item = parse_item(v);
        check_name(&item.name)?;
        let kind = if item.notreg {
            NodeKind::Other
        } else {
            NodeKind::File
        };
        // Re-derive hard-link sharing so imported totals dedupe like a live scan.
        let shared = item.hlink && !seen.insert((item.dev, item.ino));
        tree.import_child(
            parent,
            item.name.into(),
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
    }
    Ok(())
}

/// Names become real paths (`Tree::path_of`) that delete, open and the shell act on, so a
/// crafted dump must not be able to make an entry resolve anywhere but where it is shown. A
/// child name is exactly one path component: non-empty, not `.` or `..`, no `/`, no NUL.
fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(bad(&format!("invalid entry name {name:?}")));
    }
    Ok(())
}

/// The root name is a whole path, but it still may not be empty, contain a NUL, or climb out
/// with a `..` component.
fn check_root_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains('\0') || name.split('/').any(|c| c == "..") {
        return Err(bad(&format!("invalid root name {name:?}")));
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
