//! Turning transcript blocks into styled terminal lines: markdown, diffs,
//! wrapping and clipping.

use crate::ui::theme;
use crate::ui::theme::{BULLET, FENCE, QUOTE, RULE};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A line as it can safely be drawn: tabs opened out, escape sequences and
/// control characters taken out of it.
///
/// Almost nothing thoth draws was written by thoth. `cargo test`, `git` and
/// `npm` all emit colour the moment they think a terminal is listening, and
/// the shell tool captures that verbatim; a file, a web page or a model's
/// reply can carry anything at all. Drawn as they arrive, `\x1b[31m` repaints
/// the rest of the screen, `\x1b[2J` wipes it, an OSC sequence retitles the
/// window, and a right-to-left override makes a diff read as the opposite of
/// what it will do. The terminal answers to thoth, not to the last program it
/// happened to run.
pub fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => out.push_str("    "),
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then one byte in 0x40..=0x7e ends it
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: runs to a bell or to ESC \
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // anything else after ESC is a two-character sequence and
                // leaves with it
                _ => {}
            },
            // the bidi overrides are not formatting, they are a way to show
            // one thing and mean another, which is the whole of the trojan
            // source trick
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            // a line break is the one control character with a meaning here.
            // The renderers split on it long before this and never see one;
            // a paste is full of them and they are the point
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn fmt_k(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// 45 -> "45s", 185 -> "3m 5s".
pub fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// "http://localhost:11434/v1" -> "localhost:11434": the scheme and version
/// suffix are noise in the header.
pub fn short_url(url: &str) -> String {
    url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches("/v1")
        .trim_end_matches('/')
        .to_string()
}

/// Money, with enough decimals to be worth printing: a session that has
/// cost half a cent should not read "$0.00".
pub fn fmt_usd(v: f64) -> String {
    if v < 1.0 {
        format!("${v:.3}")
    } else {
        format!("${v:.2}")
    }
}

/// Path with the home directory folded back to `~`, so the banner does not
/// spell out the user's account name.
pub fn home_relative(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        let rest = rest.display().to_string();
        return if rest.is_empty() {
            "~".to_string()
        } else {
            format!("~{}{rest}", std::path::MAIN_SEPARATOR)
        };
    }
    s
}

/// Renders the body of a diff/permission preview. Lines are colored by their
/// first character: '+' insert, '-' delete, ' ' context, '$' command, other = header.
pub fn render_diff_body(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let dim = theme::muted();
    for l in text.lines() {
        let l = printable(l);
        let style = match l.chars().next() {
            Some('+') => Style::default().fg(theme::ADDED),
            Some('-') => theme::danger(),
            Some(' ') | Some('.') => dim,
            Some('$') => theme::bold(),
            _ => Style::default().fg(theme::BUSY),
        };
        // a command is the one line here that wraps instead of being cut.
        // Everything else is code, where the columns mean something and a
        // wrapped line reads worse than a clipped one; a command is a thing
        // the user is about to say yes to, and half of it is not enough to
        // say yes to
        if l.starts_with('$') {
            // and it breaks at spaces only: `--test-threads=1` split across
            // two rows is a command the user cannot read back to themselves
            let w = textwrap::Options::new(width.saturating_sub(4).max(8))
                .word_splitter(textwrap::WordSplitter::NoHyphenation);
            for (n, piece) in textwrap::wrap(&l, w).iter().enumerate() {
                out.push(Line::from(Span::styled(
                    format!("  {}{piece}", if n == 0 { "" } else { "  " }),
                    style,
                )));
            }
            continue;
        }
        out.push(Line::from(Span::styled(
            format!("  {}", clip(&l, width.saturating_sub(2))),
            style,
        )));
    }
}

/// Lightweight markdown rendering: code fences, headings, quotes, lists,
/// `inline code` and **bold**.
///
/// The markers are read and then dropped, not printed. `###` in front of a
/// heading is instruction to the renderer, and leaving it on screen next to a
/// heading that is already bold says the same thing twice in the uglier of
/// the two ways.
pub fn render_markdown(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let mut in_fence = false;
    for src in text.lines() {
        let trimmed = src.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            // opening: a rule with the language on it, so a block of code is
            // bounded by something quieter than three backticks
            let lang = trimmed.trim_start_matches('`').trim();
            let label = if in_fence && !lang.is_empty() {
                format!("{FENCE}{FENCE} {} ", printable(lang))
            } else {
                format!("{FENCE}{FENCE}")
            };
            let rule = FENCE.repeat(width.saturating_sub(label.chars().count()).min(24));
            out.push(Line::from(Span::styled(
                clip(&format!("{label}{rule}"), width),
                theme::muted(),
            )));
            continue;
        }
        if in_fence {
            out.push(Line::from(Span::styled(
                format!("  {}", clip(&printable(src), width.saturating_sub(2))),
                Style::default().fg(theme::CODE),
            )));
            continue;
        }
        if src.is_empty() {
            out.push(Line::default());
            continue;
        }
        // a thematic break is a line, and drawing it as one costs nothing
        if matches!(trimmed, "---" | "***" | "___" | "* * *") {
            out.push(Line::from(Span::styled(
                RULE.repeat(width.min(24)),
                theme::muted(),
            )));
            continue;
        }
        let indent = src.len() - trimmed.len();
        let pad = " ".repeat(indent.min(width / 2));
        if let Some(rest) = heading(trimmed) {
            out.push(Line::from(inline_spans(
                &clip(&printable(rest), width),
                theme::accent().add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        let (marker, body, base) = if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            (format!("{pad}{BULLET} "), rest, Style::default())
        } else if let Some(rest) = trimmed.strip_prefix("> ") {
            (format!("{pad}{QUOTE} "), rest, theme::muted())
        } else {
            (pad.clone(), trimmed, Style::default())
        };
        // the marker is drawn once and the wrapped rest lines up under it, so
        // a list item that runs onto a second line still reads as one item
        let hang = marker.chars().count();
        let body_w = width.saturating_sub(hang).max(2);
        for (n, piece) in textwrap::wrap(&printable(body), body_w).iter().enumerate() {
            let mut spans = vec![Span::styled(
                if n == 0 {
                    marker.clone()
                } else {
                    " ".repeat(hang)
                },
                theme::muted(),
            )];
            spans.extend(inline_spans(piece, base));
            out.push(Line::from(spans));
        }
    }
}

/// `## Heading` -> `Heading`. One to six hashes and then a space, which is
/// what stops `#!/bin/sh` and `#42` from being read as one.
fn heading(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        trimmed[hashes..].strip_prefix(' ').map(str::trim_start)
    } else {
        None
    }
}

/// Splits a single line into styled spans for `code` and **bold** markers.
pub fn inline_spans(s: &str, base: Style) -> Vec<Span<'static>> {
    let code = base.fg(theme::BUSY);
    let bold = base.add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let flush = |spans: &mut Vec<Span<'static>>, cur: &mut String| {
        if !cur.is_empty() {
            spans.push(Span::styled(std::mem::take(cur), base));
        }
    };
    while let Some(c) = chars.next() {
        if c == '`' {
            let mut inner = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '`' {
                    closed = true;
                    break;
                }
                inner.push(c2);
            }
            if closed && !inner.is_empty() {
                flush(&mut spans, &mut cur);
                spans.push(Span::styled(inner, code));
            } else {
                cur.push('`');
                cur.push_str(&inner);
            }
        } else if c == '*' && chars.peek() == Some(&'*') {
            chars.next();
            let mut inner = String::new();
            let mut closed = false;
            while let Some(c2) = chars.next() {
                if c2 == '*' && chars.peek() == Some(&'*') {
                    chars.next();
                    closed = true;
                    break;
                }
                inner.push(c2);
            }
            if closed && !inner.is_empty() {
                flush(&mut spans, &mut cur);
                spans.push(Span::styled(inner, bold));
            } else {
                cur.push_str("**");
                cur.push_str(&inner);
            }
        } else {
            cur.push(c);
        }
    }
    flush(&mut spans, &mut cur);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

pub fn clip(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(1);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

pub fn wrap_into(
    out: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    prefix: &str,
    prefix_style: Style,
    style: Style,
) {
    let pw = UnicodeWidthStr::width(prefix);
    let w = width.saturating_sub(pw).max(2);
    let mut first = true;
    for src in text.lines() {
        if src.is_empty() {
            out.push(Line::default());
            continue;
        }
        for piece in textwrap::wrap(&printable(src), w) {
            let mut spans = Vec::new();
            if first {
                spans.push(Span::styled(prefix.to_string(), prefix_style));
                first = false;
            } else if pw > 0 {
                spans.push(Span::raw(" ".repeat(pw)));
            }
            spans.push(Span::styled(piece.into_owned(), style));
            out.push(Line::from(spans));
        }
    }
    if first {
        out.push(Line::from(Span::styled(prefix.to_string(), prefix_style)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_and_urls() {
        assert_eq!(fmt_k(999), "999");
        assert_eq!(fmt_k(15_400), "15.4k");
        assert_eq!(fmt_elapsed(45), "45s");
        assert_eq!(fmt_elapsed(185), "3m 5s");
        assert_eq!(short_url("http://localhost:11434/v1"), "localhost:11434");
        assert_eq!(short_url("https://api.example.com/"), "api.example.com");
    }

    #[test]
    fn clips_by_display_width_not_bytes() {
        // wide characters count as two columns
        assert_eq!(clip("ทดสอบ", 10), "ทดสอบ");
        let clipped = clip("aaaaaaaaaa", 5);
        assert!(clipped.ends_with('…'));
        assert!(clipped.chars().count() <= 5);
    }

    /// Nothing a tool, a file or a model sends may reach the terminal as an
    /// instruction to it.
    #[test]
    fn escape_sequences_never_reach_the_terminal() {
        // colour, and the reset after it
        assert_eq!(printable("\u{1b}[31merror\u{1b}[0m: no"), "error: no");
        // a clear-screen, a cursor move, a scroll region
        assert_eq!(printable("a\u{1b}[2Jb\u{1b}[10;20Hc\u{1b}[1;5r"), "abc");
        // an OSC that retitles the window, ended either way it may be ended
        assert_eq!(printable("x\u{1b}]0;pwned\u{7}y"), "xy");
        assert_eq!(printable("x\u{1b}]0;pwned\u{1b}\\y"), "xy");
        // a two-character escape
        assert_eq!(printable("a\u{1b}=b"), "ab");
        // a bell, a backspace, a stray carriage return
        assert_eq!(printable("a\u{7}b\u{8}c\rd"), "abcd");
        // and the overrides that make text read as its own reverse
        assert_eq!(printable("del\u{202e}txt.exe"), "deltxt.exe");
        // tabs open out, and a line break is left alone: a paste is made of
        // them and the renderers have split on them long before this
        assert_eq!(printable("a\tb\nc"), "a    b\nc");
        // an escape at the very end runs out of input rather than off the end
        assert_eq!(printable("a\u{1b}"), "a");
        assert_eq!(printable("a\u{1b}["), "a");
    }

    fn md(text: &str, width: usize) -> Vec<String> {
        let mut out = Vec::new();
        render_markdown(&mut out, text, width);
        out.iter().map(|l| l.to_string()).collect()
    }

    /// A marker is an instruction to the renderer. Printing it as well as
    /// obeying it says the same thing twice, in the uglier of the two ways.
    #[test]
    fn markdown_markers_are_obeyed_not_printed() {
        assert_eq!(md("## Heading", 40), ["Heading"]);
        // but only where it is a heading: a shebang and an issue number are
        // not, and neither is a run of seven
        assert_eq!(md("#!/bin/sh", 40), ["#!/bin/sh"]);
        assert_eq!(md("#42 is open", 40), ["#42 is open"]);
        assert_eq!(md("####### deep", 40), ["####### deep"]);

        assert_eq!(
            md("- one\n* two\n+ three", 40),
            ["• one", "• two", "• three"]
        );
        assert_eq!(md("> quoted", 40), ["│ quoted"]);
        // a numbered list keeps its numbers, which carry meaning
        assert_eq!(md("1. first", 40), ["1. first"]);
        // the rest of a wrapped item lines up under the first word, not
        // under the bullet
        assert_eq!(
            md("- aaaa bbbb cccc dddd", 12),
            ["• aaaa bbbb", "  cccc dddd"]
        );
        // an indented item keeps its level
        assert_eq!(md("  - nested", 40), ["  • nested"]);

        let fence = md("```rust\nfn x() {}\n```", 40);
        assert!(fence[0].starts_with("╌╌ rust"), "{fence:?}");
        assert_eq!(fence[1], "  fn x() {}");
        assert!(!fence[2].contains('`'), "{fence:?}");
        assert!(md("---", 40)[0].starts_with('─'));
    }
}
