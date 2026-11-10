//! Matching what is typed against a name, and ordering the matches.
//!
//! A match is graded in tiers, best first: the name starts with what is
//! typed; a part of it does (`items` in `order_items`, `Date` in `toDate`);
//! its letters appear in order from its first (`oi` for `order_items`); it
//! contains what is typed. The loose tiers need a few letters typed, so one
//! letter does not match half the catalog.

/// How well a name matched; lower is better.
pub(super) type Tier = u8;

/// The tier `query` (lower case) matches `name` in, if any. `lower` is
/// `name` lower-cased. Without `loose`, only a prefix counts, as for
/// keywords, which are short and many.
pub(super) fn tier(query: &str, name: &str, lower: &str, loose: bool) -> Option<Tier> {
    if lower.starts_with(query) {
        return Some(0);
    }
    if !loose {
        return None;
    }
    // Byte offsets in `name` hold in `lower` only when lowering kept every
    // character's length; ASCII names, nearly all of them, do.
    if name.is_ascii() && segment_starts(name).any(|ix| lower[ix..].starts_with(query)) {
        return Some(1);
    }
    let mut chars = query.chars();
    if query.len() >= 2
        && chars.next() == lower.chars().next()
        && is_subsequence(
            chars.as_str(),
            &lower[lower.chars().next().map_or(0, char::len_utf8)..],
        )
    {
        return Some(2);
    }
    if query.len() >= 3 && lower.contains(query) {
        return Some(3);
    }
    None
}

/// Where the words in an identifier start, after the first: after `_`,
/// `$` or a digit run, and at a capital after a small letter (`toDate`).
fn segment_starts(name: &str) -> impl Iterator<Item = usize> + '_ {
    let bytes = name.as_bytes();
    (1..bytes.len()).filter(move |&ix| {
        let (prev, here) = (bytes[ix - 1], bytes[ix]);
        here.is_ascii_alphanumeric()
            && (matches!(prev, b'_' | b'$' | b' ' | b'-')
                || (here.is_ascii_uppercase() && prev.is_ascii_lowercase())
                || (here.is_ascii_alphabetic() && prev.is_ascii_digit()))
    })
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle.chars().all(|c| haystack.any(|h| h == c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(query: &str, name: &str) -> Option<Tier> {
        tier(query, name, &name.to_lowercase(), true)
    }

    #[test]
    fn grades_matches() {
        assert_eq!(t("", "orders"), Some(0));
        assert_eq!(t("ord", "Orders"), Some(0));
        assert_eq!(t("items", "order_items"), Some(1));
        assert_eq!(t("date", "toDate"), Some(1));
        assert_eq!(t("oi", "order_items"), Some(2));
        assert_eq!(t("rde", "orders"), Some(3));
        assert_eq!(t("rd", "orders"), None);
        assert_eq!(t("x", "orders"), None);
        assert_eq!(tier("items", "order_items", "order_items", false), None);
    }

    #[test]
    fn handles_names_beyond_ascii() {
        assert_eq!(t("주문", "주문내역"), Some(0));
        assert_eq!(t("내역", "주문내역"), Some(3));
    }
}
