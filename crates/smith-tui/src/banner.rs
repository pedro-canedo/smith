const RAW: &str = include_str!("../../../ASCII - smith.md");

/// The project's ASCII banner, with the surrounding markdown code fence
/// stripped and every row trimmed to the same width.
///
/// The per-line trim is not cosmetic. The splash draws the banner through a
/// centred `Paragraph`, and ratatui centres each line by *its own* width — so
/// one row carrying a stray space past the box's right border (the source
/// file had exactly one) is offset half a cell from its neighbours, and the
/// frame it is supposed to close comes out ragged.
pub fn banner() -> String {
    RAW.lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    /// A box is a box: every row the same width, or the borders do not line
    /// up once each is centred independently.
    #[test]
    fn every_banner_row_is_the_same_width() {
        let banner = super::banner();
        let widths: Vec<usize> = banner.lines().map(UnicodeWidthStr::width).collect();
        let first = widths[0];
        assert!(
            widths.iter().all(|w| *w == first),
            "ragged banner rows: {widths:?}"
        );
    }
}
