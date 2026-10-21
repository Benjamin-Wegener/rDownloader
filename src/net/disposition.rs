//! `Content-Disposition` file name parsing (RFC 6266, RFC 5987).
//!
//! Only the `filename` and `filename*` parameters matter for downloads.
//! `filename*` (charset + percent encoding) wins over `filename`, as required
//! by RFC 6266. The result is reduced to a safe base name so a header can
//! never escape the download directory.

/// Returns a safe file name from a `Content-Disposition` header value.
pub(crate) fn filename(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;

    // The first segment is the disposition type (`attachment` / `inline`).
    for segment in segments(value).into_iter().skip(1) {
        let Some((name, raw)) = segment.split_once('=') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let raw = raw.trim();
        match name.as_str() {
            "filename" if plain.is_none() => plain = Some(unquote(raw)),
            "filename*" if extended.is_none() => extended = Some(unquote(raw)),
            _ => {}
        }
    }

    let name = extended
        .as_deref()
        .and_then(decode_extended)
        .or(plain)
        .and_then(|name| sanitize(&name))?;
    (!name.is_empty()).then_some(name)
}

/// Splits on `;`, ignoring separators inside quoted strings.
fn segments(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;

    for (i, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out
}

/// Removes surrounding quotes and backslash escapes from a parameter value.
fn unquote(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);

    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Decodes an RFC 5987 `charset'language'percent-encoded` value.
fn decode_extended(value: &str) -> Option<String> {
    let (charset, rest) = value.split_once('\'')?;
    let (_language, encoded) = rest.split_once('\'')?;
    let bytes = percent_decode(encoded);

    match charset.trim().to_ascii_lowercase().as_str() {
        "iso-8859-1" | "latin1" | "us-ascii" | "ascii" => {
            Some(bytes.iter().map(|&b| b as char).collect())
        }
        _ => String::from_utf8(bytes).ok(),
    }
}

/// Percent-decodes bytes; invalid escapes are kept literally.
fn percent_decode(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(high << 4 | low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Keeps only the base name and drops control characters.
fn sanitize(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base.chars().filter(|ch| !ch.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_quoted_names() {
        assert_eq!(filename("attachment").as_deref(), None);
        assert_eq!(
            filename("attachment; filename=plain.bin").as_deref(),
            Some("plain.bin")
        );
        assert_eq!(
            filename("attachment; filename=\"quoted name.pdf\"").as_deref(),
            Some("quoted name.pdf")
        );
        assert_eq!(
            filename("inline; filename=\"a;b.txt\"").as_deref(),
            Some("a;b.txt")
        );
        assert_eq!(
            filename("attachment; filename=\"a\\\"b.txt\"").as_deref(),
            Some("a\"b.txt")
        );
    }

    #[test]
    fn extended_name_wins_and_is_decoded() {
        let value = "attachment; filename=\"fallback.txt\"; filename*=UTF-8''na%C3%AFve%20file.txt";
        assert_eq!(filename(value).as_deref(), Some("naïve file.txt"));

        let latin1 = "attachment; filename*=iso-8859-1''caf%E9.txt";
        assert_eq!(filename(latin1).as_deref(), Some("café.txt"));

        assert_eq!(
            filename("attachment; filename*=UTF-8''bad%ZZ.txt").as_deref(),
            Some("bad%ZZ.txt")
        );
        assert_eq!(
            filename("attachment; filename*=UTF-8''broken").as_deref(),
            Some("broken")
        );
    }

    #[test]
    fn keeps_only_a_safe_base_name() {
        assert_eq!(
            filename("attachment; filename=\"../../etc/passwd\"").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            filename("attachment; filename=\"dir\\\\name.txt\"").as_deref(),
            Some("name.txt")
        );
        assert_eq!(filename("attachment; filename=\"..\"").as_deref(), None);
        assert_eq!(filename("attachment; filename=\"\"").as_deref(), None);
        assert_eq!(
            filename("attachment; filename=\"a\u{0}b\"").as_deref(),
            Some("ab")
        );
    }

    #[test]
    fn case_insensitive_parameter_names() {
        assert_eq!(
            filename("ATTACHMENT; FileName=Upper.TXT").as_deref(),
            Some("Upper.TXT")
        );
    }
}
