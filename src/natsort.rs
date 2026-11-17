//! Natural order for names: a run of digits counts as the number it spells, so `file2` comes
//! before `file10`. ncdu 2 orders names this way unless it is told `--disable-natsort`.

use std::cmp::Ordering;

/// Compare two names in natural order.
///
/// Everything but digits is compared byte by byte, which for UTF-8 is the order of the
/// characters. Two runs of digits are compared as numbers of any length; leading zeros do
/// not count. Names that differ only in such zeros (`a1`, `a01`) fall back to byte order, so
/// that two names are equal only if they are the same.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    // Most names part ways at a place where no number is involved: neither of the two bytes
    // is a digit, and none stands right before them. Up to there both names have the same
    // numbers, so those two bytes decide, as in plain byte order. Sorting a large directory
    // is mostly this.
    let same = x.iter().zip(y).take_while(|(p, q)| p == q).count();
    let digit_at = |bytes: &[u8], at: usize| bytes.get(at).is_some_and(u8::is_ascii_digit);
    let number_near = digit_at(x, same) || digit_at(y, same) || (same > 0 && digit_at(x, same - 1));
    if !number_near {
        return x[same..].cmp(&y[same..]);
    }
    // A number is involved. The names agree up to `same`, so the comparison starts where
    // that number does.
    let mut i = same;
    while i > 0 && x[i - 1].is_ascii_digit() {
        i -= 1;
    }
    let mut j = i;
    loop {
        match (x.get(i), y.get(j)) {
            (None, None) => return x.cmp(y),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let (run_x, run_y) = (digits(&x[i..]), digits(&y[j..]));
                let (num_x, num_y) = (without_zeros(run_x), without_zeros(run_y));
                // Without leading zeros the longer number is the larger one.
                let order = num_x.len().cmp(&num_y.len()).then_with(|| num_x.cmp(num_y));
                if order != Ordering::Equal {
                    return order;
                }
                i += run_x.len();
                j += run_y.len();
            }
            (Some(p), Some(q)) if p != q => return p.cmp(q),
            _ => {
                i += 1;
                j += 1;
            }
        }
    }
}

/// The first eight bytes of a name as one number, which orders like those bytes. Sorting
/// keeps it next to each entry, so that most comparisons need not look at the names, which
/// lie all over the tree's memory.
pub fn prefix(name: &str) -> u64 {
    let mut bytes = [0u8; 8];
    let len = name.len().min(8);
    bytes[..len].copy_from_slice(&name.as_bytes()[..len]);
    u64::from_be_bytes(bytes)
}

/// The order of two names as far as their prefixes can tell, in natural or in byte order.
/// `None` where it takes the names themselves: the prefixes are equal, or (in natural order)
/// a number is involved where they part ways.
pub fn prefix_order(a: u64, b: u64, natural: bool) -> Option<Ordering> {
    if a == b {
        return None;
    }
    if natural {
        // The first byte that differs, counted from the front, and the one before it.
        let at = (a ^ b).leading_zeros() / 8;
        let digit_at = |prefix: u64, at: u32| ((prefix >> (56 - 8 * at)) as u8).is_ascii_digit();
        if digit_at(a, at) || digit_at(b, at) || (at > 0 && digit_at(a, at - 1)) {
            return None;
        }
    }
    Some(a.cmp(&b))
}

/// The digits `bytes` starts with.
fn digits(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    &bytes[..end]
}

fn without_zeros(run: &[u8]) -> &[u8] {
    let start = run.iter().position(|&b| b != b'0').unwrap_or(run.len());
    &run[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(names: &[&'static str]) -> Vec<&'static str> {
        let mut names = names.to_vec();
        names.sort_by(|a, b| natural_cmp(a, b));
        names
    }

    #[test]
    fn numbers_in_names_count_as_numbers() {
        assert_eq!(
            sorted(&["file10", "file2", "file1", "file20", "file3"]),
            ["file1", "file2", "file3", "file10", "file20"]
        );
        assert_eq!(
            sorted(&["v1.10", "v1.9", "v1.2", "v10.0", "v2.0"]),
            ["v1.2", "v1.9", "v1.10", "v2.0", "v10.0"]
        );
        // A number where the other name has a letter: by the bytes, digits first.
        assert_eq!(sorted(&["a", "1", "b2", "ba"]), ["1", "a", "b2", "ba"]);
        // Numbers longer than any integer type.
        assert_eq!(
            natural_cmp("n99999999999999999999999", "n100000000000000000000000"),
            Ordering::Less
        );
    }

    #[test]
    fn without_numbers_it_is_byte_order() {
        let mut names = vec!["beta", "Alpha", "alpha", "", "zeta", "Ärger", "a b", "a-b"];
        let mut plain = names.clone();
        plain.sort();
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, plain);
    }

    /// The shortcut for names without a number where they differ must agree with the full
    /// comparison. Every pair out of a set of awkward names is compared both ways round.
    #[test]
    fn the_order_is_consistent() {
        let names = [
            "",
            "0",
            "00",
            "1",
            "01",
            "10",
            "9",
            "a",
            "a0",
            "a00",
            "a1",
            "a01",
            "a1a",
            "a1b",
            "a01b",
            "a2",
            "a10",
            "a10b",
            "a9z",
            "ab",
            "a-1",
            "a-10",
            "a-2",
            "file",
            "file1",
            "file1x",
            "file12",
            "file2",
            "file10",
            "x1y2",
            "x1y10",
            "x01y2",
            "x1y",
            "x1z",
            "é1",
            "é10",
            "é2",
            "日本",
            "日本2",
            "日本10",
            "longer-name-1",
            "longer-name-10",
            "longer-name-2",
            "longer-nam",
            "longer-n1",
            "longer-n10",
            "longer-n2",
            "longer-",
            "12345678",
            "123456789",
            "1234567a",
            "12345678a",
            "abcdefgh",
            "abcdefgh1",
            "abcdefgh10",
            "abcdefgh2",
            "abcdefg1",
            "abcdefg10",
            "abcdefg2",
            "abcdefg",
        ];
        for a in names {
            assert_eq!(natural_cmp(a, a), Ordering::Equal, "{a:?}");
            for b in names {
                assert_eq!(
                    natural_cmp(a, b),
                    natural_cmp(b, a).reverse(),
                    "{a:?} and {b:?}"
                );
                assert_eq!(
                    natural_cmp(a, b) == Ordering::Equal,
                    a == b,
                    "{a:?} and {b:?}"
                );
                // And it is transitive.
                for c in names {
                    if natural_cmp(a, b) == Ordering::Less && natural_cmp(b, c) == Ordering::Less {
                        assert_eq!(natural_cmp(a, c), Ordering::Less, "{a:?} {b:?} {c:?}");
                    }
                }
            }
        }
        // Where the prefixes of two names say which comes first, the names say the same.
        for a in names {
            for b in names {
                for natural in [true, false] {
                    let full = if natural { natural_cmp(a, b) } else { a.cmp(b) };
                    if let Some(order) = prefix_order(prefix(a), prefix(b), natural) {
                        assert_eq!(order, full, "{a:?} and {b:?}, natural: {natural}");
                    }
                }
            }
        }
        let order = |a, b, natural| prefix_order(prefix(a), prefix(b), natural);
        assert_eq!(order("abc", "abd", true), Some(Ordering::Less));
        assert_eq!(order("ab", "abc", true), Some(Ordering::Less));
        assert_eq!(order("a2", "a10", true), None, "a number");
        assert_eq!(order("a2", "a10", false), Some(Ordering::Greater));
        assert_eq!(order("a1", "a1b", true), None, "after a number");
        assert_eq!(
            order("12345678x", "12345678y", false),
            None,
            "the same start"
        );
        assert_eq!(
            natural_cmp("file12", "file1x"),
            Ordering::Greater,
            "12 is more than 1"
        );
        assert_eq!(natural_cmp("a9z", "a10"), Ordering::Less);
    }

    #[test]
    fn a_prefix_comes_first() {
        assert_eq!(
            sorted(&["file1x", "file1", "file"]),
            ["file", "file1", "file1x"]
        );
        assert_eq!(natural_cmp("a1", "a1b"), Ordering::Less);
    }

    /// Leading zeros do not change the number. Names that differ only in them still have an
    /// order, so that sorting is the same every time.
    #[test]
    fn leading_zeros_do_not_count_but_names_stay_distinct() {
        assert_eq!(sorted(&["a010", "a9", "a0011"]), ["a9", "a010", "a0011"]);
        assert_eq!(natural_cmp("a01", "a1"), Ordering::Less);
        assert_eq!(natural_cmp("a1", "a01"), Ordering::Greater);
        assert_eq!(natural_cmp("a01", "a01"), Ordering::Equal);
        assert_eq!(natural_cmp("0", "00"), Ordering::Less);
        // Equal numbers: what follows decides.
        assert_eq!(natural_cmp("a01b", "a1a"), Ordering::Greater);
    }
}
