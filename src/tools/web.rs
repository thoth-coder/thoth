use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::Duration;

/// (api_key, cx) for Google Programmable Search, if the user configured it.
static GOOGLE: OnceLock<(String, String)> = OnceLock::new();

pub fn set_google(api_key: String, cx: String) {
    let _ = GOOGLE.set((api_key, cx));
}

const UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) thoth/0.1";
const MAX_RESULTS: usize = 8;
const MAX_FETCH_CHARS: usize = 15_000;

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(25))
        .build()
        .context("failed to build http client")
}

#[derive(Deserialize)]
pub struct SearchArgs {
    pub query: String,
}

pub async fn search(a: SearchArgs) -> Result<String> {
    if let Some((key, cx)) = GOOGLE.get() {
        match google_search(key, cx, &a.query).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                // fall back to DuckDuckGo so search still works
                let ddg = ddg_search(&a.query).await?;
                return Ok(format!(
                    "(google search failed: {e:#}; results below are from DuckDuckGo)\n\n{ddg}"
                ));
            }
        }
    }
    ddg_search(&a.query).await
}

async fn google_search(key: &str, cx: &str, query: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Item {
        title: String,
        link: String,
        #[serde(default)]
        snippet: String,
    }
    #[derive(Deserialize)]
    struct R {
        #[serde(default)]
        items: Vec<Item>,
    }
    let resp = http()?
        .get("https://www.googleapis.com/customsearch/v1")
        .query(&[("key", key), ("cx", cx), ("q", query), ("num", "8")])
        .send()
        .await
        .context("google search request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let short: String = body.chars().take(300).collect();
        bail!("google api returned {status}: {short}");
    }
    let r: R = serde_json::from_str(&body).context("unexpected google api response")?;
    if r.items.is_empty() {
        return Ok("No results found.".into());
    }
    let mut out = String::new();
    for (i, item) in r.items.iter().take(MAX_RESULTS).enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, item.title, item.link));
        if !item.snippet.is_empty() {
            out.push_str(&format!(
                "   {}\n",
                item.snippet
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
    }
    out.push_str("\nUse web_fetch with a URL to read a page.");
    Ok(out)
}

async fn ddg_search(query: &str) -> Result<String> {
    let resp = http()?
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", query)])
        .send()
        .await
        .context("web search request failed. no internet access?")?;
    let html = resp
        .error_for_status()
        .context("search engine rejected the request")?
        .text()
        .await?;

    let link_re = Regex::new(r#"(?s)class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)?;
    let snip_re = Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#)?;
    let snippets: Vec<String> = snip_re
        .captures_iter(&html)
        .map(|c| clean_text(&c[1]))
        .collect();

    let mut out = String::new();
    for (i, c) in link_re.captures_iter(&html).take(MAX_RESULTS).enumerate() {
        let url = decode_ddg_url(&c[1]);
        let title = clean_text(&c[2]);
        out.push_str(&format!("{}. {title}\n   {url}\n", i + 1));
        if let Some(s) = snippets.get(i)
            && !s.is_empty()
        {
            out.push_str(&format!("   {s}\n"));
        }
    }
    if out.is_empty() {
        return Ok("No results found.".into());
    }
    out.push_str("\nUse web_fetch with a URL to read a page.");
    Ok(out)
}

#[derive(Deserialize)]
pub struct FetchArgs {
    pub url: String,
}

pub async fn fetch(a: FetchArgs) -> Result<String> {
    if !a.url.starts_with("http://") && !a.url.starts_with("https://") {
        bail!("url must start with http:// or https://");
    }
    let resp = http()?
        .get(&a.url)
        .send()
        .await
        .with_context(|| format!("failed to fetch {}", a.url))?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.context("failed to read response body")?;
    if !status.is_success() {
        bail!("{} returned {status}", a.url);
    }

    let text = if content_type.contains("html") || looks_like_html(&body) {
        html_to_text(&body)
    } else {
        body
    };
    let mut text = text.trim().to_string();
    if text.chars().count() > MAX_FETCH_CHARS {
        text = text.chars().take(MAX_FETCH_CHARS).collect();
        text.push_str("\n... (page truncated)");
    }
    if text.is_empty() {
        text = "(page has no text content)".into();
    }
    Ok(text)
}

fn looks_like_html(s: &str) -> bool {
    let head: String = s.chars().take(512).collect::<String>().to_lowercase();
    head.contains("<html") || head.contains("<!doctype html")
}

fn html_to_text(html: &str) -> String {
    // drop non-content blocks, then tags, then decode entities
    let re_block = Regex::new(
        r"(?is)<script\b.*?</script>|<style\b.*?</style>|<noscript\b.*?</noscript>|<svg\b.*?</svg>|<head\b.*?</head>",
    )
    .unwrap();
    let s = re_block.replace_all(html, " ");
    let re_break = Regex::new(r"(?i)<(/p|/div|/li|/h[1-6]|br[^>]*|/tr)>").unwrap();
    let s = re_break.replace_all(&s, "\n");
    let re_tag = Regex::new(r"(?s)<[^>]*>").unwrap();
    let s = re_tag.replace_all(&s, " ");
    let s = decode_entities(&s);
    // collapse whitespace but keep line structure
    let mut out = String::new();
    let mut blank = 0;
    for line in s.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn clean_text(s: &str) -> String {
    let re_tag = Regex::new(r"(?s)<[^>]*>").unwrap();
    let s = re_tag.replace_all(s, "");
    decode_entities(&s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// DuckDuckGo wraps result URLs as //duckduckgo.com/l/?uddg=<encoded>&rut=...
fn decode_ddg_url(href: &str) -> String {
    let href = decode_entities(href);
    if let Some(pos) = href.find("uddg=") {
        let rest = &href[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // s.get() instead of slicing: the two bytes after '%' may be the
        // middle of a multi-byte char, and slicing there panics
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // network tests — run explicitly with: cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn ddg_returns_results() {
        let out = ddg_search("rust programming language").await.unwrap();
        assert!(out.contains("1."), "no results: {out}");
        assert!(out.contains("http"), "no links: {out}");
    }

    #[tokio::test]
    #[ignore]
    async fn google_bad_key_reports_error() {
        let err = google_search("bad-key", "bad-cx", "rust")
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("google"), "{err:#}");
    }

    #[tokio::test]
    #[ignore]
    async fn fetch_extracts_text() {
        let out = fetch(FetchArgs {
            url: "https://example.com".into(),
        })
        .await
        .unwrap();
        assert!(out.to_lowercase().contains("example domain"), "{out}");
    }
}
