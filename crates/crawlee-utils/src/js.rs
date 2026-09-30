//! Conversions that behave like their JavaScript counterparts, where Crawlee for JS relies on them.

/// `Number(text)` of JavaScript: `None` for `NaN`.
pub(crate) fn number(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return Some(0.0);
    }
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok().map(|n| n as f64);
    }
    match text {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // Rust accepts `inf` and `nan`, which JavaScript does not.
    let digits = text.trim_start_matches(['+', '-']);
    if digits.starts_with(|c: char| c.is_ascii_digit() || c == '.') { text.parse().ok() } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_like_javascript() {
        assert_eq!(number(""), Some(0.0));
        assert_eq!(number(" 1.5 "), Some(1.5));
        assert_eq!(number("0x1A"), Some(26.0));
        assert_eq!(number("1e2"), Some(100.0));
        assert_eq!(number("inf"), None);
        assert_eq!(number("abc"), None);
    }
}
