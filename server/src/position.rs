//! Conversions between LSP positions (line + UTF-16 column) and byte offsets.

use typst::syntax::Source;

pub fn to_offset(source: &Source, line: u32, character: u32) -> usize {
    let lines = source.lines();
    let Some(line_start) = lines.line_to_byte(line as usize) else {
        return source.text().len();
    };
    let line_end = lines
        .line_to_range(line as usize)
        .map(|r| {
            let text = &source.text()[r.clone()];
            r.end - (text.len() - text.trim_end_matches(['\n', '\r']).len())
        })
        .unwrap_or(source.text().len());
    let start_utf16 = lines.byte_to_utf16(line_start).unwrap_or(0);
    lines
        .utf16_to_byte(start_utf16 + character as usize)
        .unwrap_or(line_end)
        .clamp(line_start, line_end)
}

pub fn to_position(source: &Source, offset: usize) -> (u32, u32) {
    let lines = source.lines();
    let offset = offset.min(source.text().len());
    let line = lines.byte_to_line(offset).unwrap_or(0);
    let line_start = lines.line_to_byte(line).unwrap_or(0);
    let col = lines.byte_to_utf16(offset).unwrap_or(0) - lines.byte_to_utf16(line_start).unwrap_or(0);
    (line as u32, col as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_multibyte() {
        let src = Source::detached("ab\nあい😀x\n");
        // line 1: "あい😀x" — 😀 is two UTF-16 units.
        let off = to_offset(&src, 1, 4);
        assert_eq!(&src.text()[off..off + 1], "x");
        assert_eq!(to_position(&src, off), (1, 4));
        assert_eq!(to_offset(&src, 0, 99), 2);
        assert_eq!(to_offset(&src, 9, 0), src.text().len());
    }
}
