//! Decoding of character references in HTML attribute values.
//!
//! The streaming link extractor sees attribute values exactly as written in the document, so
//! `href="?a=1&amp;b=2"` has to be decoded the way an HTML5 parser would. This follows the WHATWG
//! "character reference state" rules for attribute values, using the full named-entity table from
//! `web_atoms` (the table html5ever uses).

use std::borrow::Cow;

use web_atoms::{C1_REPLACEMENTS, NAMED_ENTITIES};

/// Decodes character references in an attribute value. Values without `&` are returned borrowed.
pub fn decode_attribute_value(value: &str) -> Cow<'_, str> {
    if !value.contains('&') {
        return Cow::Borrowed(value);
    }

    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        match decode_reference(after) {
            Some((decoded, consumed)) => {
                out.push_str(&decoded);
                rest = &after[consumed..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);

    Cow::Owned(out)
}

/// Decodes the reference that follows an `&`. Returns the decoded text and the number of input
/// bytes consumed, or `None` when the `&` must be kept literally.
fn decode_reference(input: &str) -> Option<(String, usize)> {
    if let Some(numeric) = input.strip_prefix('#') {
        return decode_numeric(numeric).map(|(c, len)| (c.to_string(), len + 1));
    }
    decode_named(input)
}

fn decode_numeric(input: &str) -> Option<(char, usize)> {
    let (digits_start, radix) = match input.as_bytes().first() {
        Some(b'x' | b'X') => (1, 16),
        _ => (0, 10),
    };
    let digits = &input[digits_start..];
    let digits_len =
        digits.bytes().take_while(|b| if radix == 16 { b.is_ascii_hexdigit() } else { b.is_ascii_digit() }).count();
    if digits_len == 0 {
        return None;
    }

    // Saturate instead of overflowing; anything above U+10FFFF becomes U+FFFD anyway.
    let code = digits[..digits_len]
        .bytes()
        .fold(0u32, |acc, b| acc.saturating_mul(radix).saturating_add((b as char).to_digit(radix).unwrap_or(0)));

    let mut consumed = digits_start + digits_len;
    if input[consumed..].starts_with(';') {
        consumed += 1;
    }

    let c = match code {
        0 => '\u{FFFD}',
        0x80..=0x9F => {
            C1_REPLACEMENTS[(code - 0x80) as usize].unwrap_or_else(|| char::from_u32(code).unwrap_or('\u{FFFD}'))
        }
        _ => char::from_u32(code).unwrap_or('\u{FFFD}'),
    };
    Some((c, consumed))
}

fn decode_named(input: &str) -> Option<(String, usize)> {
    // `NAMED_ENTITIES` also contains every prefix of every name (mapped to `(0, 0)`), so the
    // longest match is found by extending the candidate while it is still a known prefix.
    let mut best: Option<(usize, (u32, u32))> = None;
    for (idx, c) in input.char_indices() {
        let end = idx + c.len_utf8();
        match NAMED_ENTITIES.get(&input[..end]) {
            Some(&(0, 0)) => {}
            Some(&codepoints) => best = Some((end, codepoints)),
            None => break,
        }
        if c == ';' {
            break;
        }
    }

    let (len, (first, second)) = best?;

    // Legacy references without a trailing `;` are not decoded in attribute values when followed
    // by `=` or an ASCII alphanumeric (so `?a=1&copy=2` stays intact).
    if !input[..len].ends_with(';')
        && input[len..].bytes().next().is_some_and(|b| b == b'=' || b.is_ascii_alphanumeric())
    {
        return None;
    }

    let mut decoded = String::new();
    decoded.extend(char::from_u32(first));
    if second != 0 {
        decoded.extend(char::from_u32(second));
    }
    Some((decoded, len))
}

#[cfg(test)]
mod tests {
    use super::decode_attribute_value as decode;

    #[test]
    fn decodes_like_an_html5_parser() {
        assert_eq!(decode("/a?x=1&amp;y=2"), "/a?x=1&y=2");
        assert_eq!(decode("/a?x=1&y=2"), "/a?x=1&y=2");
        assert_eq!(decode("&#47;path&#x2F;x"), "/path/x");
        assert_eq!(decode("&#128;"), "\u{20ac}");
        assert_eq!(decode("&#0;"), "\u{FFFD}");
        assert_eq!(decode("a&copy=2"), "a&copy=2");
        assert_eq!(decode("a&copy 2"), "a\u{a9} 2");
        // In attribute values a legacy match followed by an alphanumeric stays literal.
        assert_eq!(decode("&notit;"), "&notit;");
        assert_eq!(decode("&unknown;"), "&unknown;");
        assert_eq!(decode("tail&"), "tail&");
        assert_eq!(decode("&#;"), "&#;");
    }
}
