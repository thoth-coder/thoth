//! Putting text on the user's clipboard, with no clipboard library behind it.
//!
//! OSC 52 hands the terminal the text and lets it do the copying. The
//! terminal is already on the other end of thoth's stdout, so the same call
//! works locally, over ssh and inside tmux, and it costs no dependency and no
//! platform code. Not every terminal answers it, and some want it turned on
//! first, which is why `ctrl+t` is there as well: that hands selection back
//! to the terminal so the user can copy the way they always do.

use std::io::Write;

/// Terminals stop reading an OSC payload somewhere, and where is not
/// something they agree on. Half a megabyte of base64 is past every limit
/// worth trying, so the copy is cut and the caller says so rather than
/// sending something the terminal will silently drop on the floor.
pub const MAX_COPY_CHARS: usize = 200_000;

/// True when the whole of `text` went. False when it was cut to fit.
pub fn copy(text: &str) -> std::io::Result<bool> {
    let whole = text.chars().count() <= MAX_COPY_CHARS;
    let text: String = text.chars().take(MAX_COPY_CHARS).collect();
    let mut out = std::io::stdout();
    // `c` is the selection every terminal understands as the clipboard
    write!(out, "\u{1b}]52;c;{}\u{7}", base64(text.as_bytes()))?;
    out.flush()?;
    Ok(whole)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vectors from RFC 4648, which is the whole of what has to be right
    /// here: a terminal handed a mis-padded payload copies nothing at all.
    #[test]
    fn encodes_like_the_rfc_says() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // and the bytes past ascii, which is most of what this user copies
        assert_eq!(base64("กิน".as_bytes()), "4LiB4Li04LiZ");
        assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
    }
}
