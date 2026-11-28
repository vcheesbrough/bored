//! Which image format an upload is, decided from its bytes alone (card #67).
//!
//! The client's `Content-Type` header is never trusted: it is whatever the
//! browser or a script chose to send. The stored type, which `GET` later
//! serves back as the response's `Content-Type`, is the one these bytes prove.
//! An upload that proves none of the five formats is refused with a 415.
//!
//! Detection is deliberately shallow. It checks the signature a format starts
//! with, not that the whole file decodes: decoding would mean parsing
//! attacker-controlled data on the server for no gain, because the browser is
//! what renders it. The security of serving the bytes comes from the response
//! headers (`nosniff`, the sandbox CSP), not from here; see `super`.

/// The five formats the image route accepts.
///
/// `Copy` because it is a plain tag with no data, cheap to pass by value.
/// `strum::EnumIter` lets the tests walk every variant without a hand-kept
/// list that could fall out of date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub(crate) enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
    Svg,
}

impl ImageType {
    /// The MIME type: what is stored in `images.content_type`, sent as the
    /// response's `Content-Type`, and recorded as the `bored.image.content_type`
    /// span attribute. A fixed set of five literals, so it is safe as a
    /// telemetry value (it cannot grow with user input).
    pub(crate) const fn mime(self) -> &'static str {
        // No `_ =>` arm on purpose: a new variant must be given a MIME type
        // here before the crate compiles.
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Svg => "image/svg+xml",
        }
    }

    /// The type a stored `content_type` names, or `None` for anything else.
    ///
    /// Used on the way *out*: the GET route only ever emits a MIME type from
    /// this fixed set, even if a row were somehow written with another value.
    pub(crate) fn from_mime(mime: &str) -> Option<Self> {
        // `iter()` comes from `strum::IntoEnumIterator`; `find` returns the
        // first variant whose MIME type matches.
        use strum::IntoEnumIterator as _;
        Self::iter().find(|image_type| image_type.mime() == mime)
    }
}

/// The format `bytes` is, or `None` when it is none of the five.
///
/// The binary formats are checked first, by their fixed leading bytes
/// ("magic numbers"). Only if none matches is the upload tried as SVG, which
/// is text and needs a small parse.
pub(crate) fn detect(bytes: &[u8]) -> Option<ImageType> {
    // `starts_with` on a byte slice compares a prefix; a too-short input is
    // simply "does not start with", never a panic.
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageType::Png);
    }
    // Every JPEG starts with the SOI marker `FF D8` followed by the next
    // marker's `FF`.
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(ImageType::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageType::Gif);
    }
    // WebP is a RIFF container: `RIFF`, a 4-byte little-endian length, then
    // the form type `WEBP`. `get(8..12)` is `None` when the input is shorter
    // than 12 bytes, so a truncated file cannot match.
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        return Some(ImageType::Webp);
    }
    if is_svg(bytes) {
        return Some(ImageType::Svg);
    }
    None
}

/// Whether `bytes` is an SVG document: valid UTF-8 whose root element is
/// `<svg`.
///
/// Before the root element an XML document may have a byte-order mark,
/// whitespace, an XML declaration (`<?xml …?>`) or other processing
/// instructions, comments (`<!-- … -->`) and a `<!DOCTYPE …>`, in any
/// order. Those are skipped; the first element must then be `svg`. An HTML
/// page (`<!DOCTYPE html><html>…`) fails here, because its root is `html`.
fn is_svg(bytes: &[u8]) -> bool {
    // The whole file must be UTF-8 (XML's default encoding), not just the
    // prolog. `from_utf8` checks every byte; `let … else` returns early when
    // it fails.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    // `strip_prefix` returns the rest of the string after the prefix, or
    // `None` if it is not there; `unwrap_or(text)` keeps the original then.
    let mut rest = text.strip_prefix('\u{feff}').unwrap_or(text);
    loop {
        // XML whitespace is exactly space, tab, CR and LF.
        rest = rest.trim_start_matches([' ', '\t', '\r', '\n']);
        if let Some(after) = rest.strip_prefix("<?") {
            // An XML declaration or a processing instruction: skip past `?>`.
            match skip_past(after, "?>") {
                Some(next) => rest = next,
                None => return false,
            }
        } else if let Some(after) = rest.strip_prefix("<!--") {
            match skip_past(after, "-->") {
                Some(next) => rest = next,
                None => return false,
            }
        } else if let Some(after) = rest.strip_prefix("<!DOCTYPE") {
            match skip_doctype(after) {
                Some(next) => rest = next,
                None => return false,
            }
        } else {
            // The first thing that is not prolog: it must open the root
            // element `svg`, and the name must end right there (so `<svgx>`
            // is not an SVG).
            let Some(after) = rest.strip_prefix("<svg") else {
                return false;
            };
            // `chars().next()` is the character after `<svg`; `matches!`
            // tests it against a pattern. `None` (the file ends there) fails.
            return matches!(
                after.chars().next(),
                Some(' ' | '\t' | '\r' | '\n' | '>' | '/')
            );
        }
    }
}

/// The text after the first `end` in `text`, or `None` if `end` never comes.
fn skip_past<'a>(text: &'a str, end: &str) -> Option<&'a str> {
    // `find` gives the byte offset where `end` starts; slicing from
    // `offset + end.len()` lands just after it, always on a character
    // boundary because `end` is ASCII.
    text.find(end).map(|offset| &text[offset + end.len()..])
}

/// The text after a `<!DOCTYPE …>`, given what follows `<!DOCTYPE`.
///
/// A DOCTYPE may carry an internal subset in square brackets, which can itself
/// contain `>` (`<!DOCTYPE svg [ <!ENTITY x "a>b"> ]>`), so the closing `>`
/// is the first one *after* the `]` when there is a `[`.
fn skip_doctype(text: &str) -> Option<&str> {
    let close = text.find('>')?;
    match text.find('[') {
        // A `[` before the first `>`: the subset ends at `]`, and the
        // declaration at the next `>` after that.
        Some(open) if open < close => {
            let after_subset = skip_past(&text[open..], "]")?;
            skip_past(after_subset, ">")
        }
        _ => Some(&text[close + 1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures are written out by hand from each format's specification,
    // never produced by `detect` itself (an independent oracle).

    /// PNG: the 8-byte signature from the PNG spec, then the start of IHDR.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";
    /// JPEG: SOI (`FF D8`) then an APP0 marker (`FF E0`).
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00";
    const GIF87: &[u8] = b"GIF87a\x01\x00\x01\x00";
    const GIF89: &[u8] = b"GIF89a\x01\x00\x01\x00";
    /// WebP: `RIFF`, a length, `WEBP`, then a `VP8 ` chunk header.
    const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

    #[test]
    fn binary_formats_are_detected_by_their_signature() {
        assert_eq!(detect(PNG), Some(ImageType::Png));
        assert_eq!(detect(JPEG), Some(ImageType::Jpeg));
        assert_eq!(detect(GIF87), Some(ImageType::Gif));
        assert_eq!(detect(GIF89), Some(ImageType::Gif));
        assert_eq!(detect(WEBP), Some(ImageType::Webp));
    }

    #[test]
    fn svg_is_detected_with_and_without_a_prolog() {
        let cases: &[&str] = &[
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            "<svg>",
            "<svg\nwidth=\"1\"></svg>",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg></svg>",
            "\u{feff}<svg></svg>",
            "  \r\n\t<svg></svg>",
            "<!-- made by hand --><svg></svg>",
            "<?xml version=\"1.0\"?><!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \
             \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\"><svg/>",
            "<!DOCTYPE svg [ <!ENTITY arrow \"a>b\"> ]>\n<svg></svg>",
            "<?xml-stylesheet href=\"a.css\"?><svg></svg>",
        ];
        for case in cases {
            assert_eq!(detect(case.as_bytes()), Some(ImageType::Svg), "{case:?}");
        }
    }

    #[test]
    fn anything_else_is_rejected() {
        let cases: &[&[u8]] = &[
            b"",
            b"<!DOCTYPE html><html><body>hi</body></html>",
            b"<html><svg></svg></html>",
            b"<?xml version=\"1.0\"?><root/>",
            b"<svgx></svgx>",
            b"<svg",
            b"<?xml version=\"1.0\"",
            b"<!-- never closed <svg>",
            b"<!DOCTYPE svg [ unterminated <svg>",
            // Truncated binary signatures.
            b"\x89PNG\r\n",
            b"\xff\xd8",
            b"GIF8",
            b"RIFF\x24\x00\x00\x00WEB",
            // A RIFF file that is not WebP (a WAV header).
            b"RIFF\x24\x00\x00\x00WAVEfmt ",
            b"plain text",
            b"%PDF-1.7",
        ];
        for case in cases {
            assert_eq!(detect(case), None, "{:?}", String::from_utf8_lossy(case));
        }
    }

    #[test]
    fn svg_must_be_valid_utf8_throughout() {
        // A good prolog and root, then a byte that is never valid UTF-8.
        let mut bytes = b"<svg>".to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b"</svg>");
        assert_eq!(detect(&bytes), None);
        // Control: the same document without the bad byte is accepted.
        assert_eq!(detect(b"<svg></svg>"), Some(ImageType::Svg));
    }

    #[test]
    fn mime_types_are_the_five_written_out_by_hand() {
        use strum::IntoEnumIterator as _;
        let mimes: Vec<&str> = ImageType::iter().map(ImageType::mime).collect();
        assert_eq!(
            mimes,
            [
                "image/png",
                "image/jpeg",
                "image/gif",
                "image/webp",
                "image/svg+xml"
            ]
        );
        // Each round-trips through `from_mime`, and nothing else does.
        for image_type in ImageType::iter() {
            assert_eq!(ImageType::from_mime(image_type.mime()), Some(image_type));
        }
        assert_eq!(ImageType::from_mime("text/html"), None);
        assert_eq!(ImageType::from_mime("image/PNG"), None);
    }
}
