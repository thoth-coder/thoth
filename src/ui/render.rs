//! Turning transcript blocks into styled terminal lines: markdown, diffs,
//! wrapping and clipping.

use crate::ui::theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn expand_tabs(s: &str) -> String {
    s.replace('\t', "    ")
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

/// Renders the body of a diff/permission preview. Lines are colored by their
/// first character: '+' insert, '-' delete, ' ' context, '$' command, other = header.
pub fn render_diff_body(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let dim = theme::muted();
    for l in text.lines() {
        let l = expand_tabs(l);
        let style = match l.chars().next() {
            Some('+') => Style::default().fg(theme::ADDED),
            Some('-') => theme::danger(),
            Some(' ') | Some('.') => dim,
            Some('$') => theme::bold(),
            _ => Style::default().fg(theme::BUSY),
        };
        out.push(Line::from(Span::styled(
            format!("  {}", clip(&l, width.saturating_sub(2))),
            style,
        )));
    }
}

/// Lightweight markdown rendering: code fences, headings, quotes,
/// `inline code` and **bold**.
pub fn render_markdown(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let mut in_fence = false;
    for src in text.lines() {
        let trimmed = src.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push(Line::from(Span::styled(
                clip(&expand_tabs(src), width),
                theme::muted(),
            )));
            continue;
        }
        if in_fence {
            out.push(Line::from(Span::styled(
                format!("  {}", clip(&expand_tabs(src), width.saturating_sub(2))),
                Style::default().fg(theme::CODE),
            )));
            continue;
        }
        if src.is_empty() {
            out.push(Line::default());
            continue;
        }
        let base = if trimmed.starts_with('#') {
            theme::accent().add_modifier(Modifier::BOLD)
        } else if trimmed.starts_with('>') {
            theme::muted()
        } else {
            Style::default()
        };
        for piece in textwrap::wrap(&expand_tabs(src), width.max(2)) {
            out.push(Line::from(inline_spans(&piece, base)));
        }
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
        for piece in textwrap::wrap(src, w) {
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
}
