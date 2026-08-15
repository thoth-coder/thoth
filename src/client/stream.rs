//! What has to be read out of a stream of text: `<think>` spans that belong
//! in the reasoning channel, and tool calls a model wrote as text instead of
//! sending as tool_calls. Shared by the OpenAI and Ollama transports;
//! Anthropic sends proper blocks and needs none of it.

use super::{AssistantTurn, FunctionCall, StreamEvent, ToolCall};
use serde_json::{Value, json};

/// Routes `<think>…</think>` spans that some models emit inside `content`
/// to reasoning instead. Tags can be split across stream chunks.
#[derive(Default)]
struct ThinkFilter {
    buf: String,
    in_think: bool,
}

impl ThinkFilter {
    /// Returns (is_reasoning, text) pieces ready to emit.
    fn push(&mut self, t: &str) -> Vec<(bool, String)> {
        self.buf.push_str(t);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(pos) = self.buf.find(tag) {
                if pos > 0 {
                    out.push((self.in_think, self.buf[..pos].to_string()));
                }
                self.buf.drain(..pos + tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            // hold back a suffix that could be the start of a split tag
            let hold = longest_suffix_prefix(&self.buf, tag);
            let emit = self.buf.len() - hold;
            if emit > 0 {
                let text: String = self.buf.drain(..emit).collect();
                out.push((self.in_think, text));
            }
            break;
        }
        out
    }

    fn flush(&mut self) -> Vec<(bool, String)> {
        if self.buf.is_empty() {
            Vec::new()
        } else {
            vec![(self.in_think, std::mem::take(&mut self.buf))]
        }
    }
}

/// A tool call the model wrote as text instead of sending as `tool_calls`.
/// Plenty of self-hosted models do this, and without it thoth just prints
/// the json and stops, which reads as "it talks about editing the file but
/// never does".
///
/// The filter holds a span back only while it could still be a call, and
/// hands the text back untouched the moment it turns out not to be one, so
/// nothing the model wrote can be swallowed. A json object only counts when
/// it names a tool that actually exists.
struct ToolTextFilter {
    names: Vec<String>,
    buf: String,
    /// The buffer starts with a span that might be a call.
    holding: bool,
}

const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";

enum Taken {
    Call(ToolCall),
    /// Not a call after all: this many bytes go back to the transcript.
    Text(usize),
    /// The span is not finished yet.
    More,
}

impl ToolTextFilter {
    fn new(tools: &Value) -> Self {
        let names = tools
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| Some(t.get("function")?.get("name")?.as_str()?.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            names,
            buf: String::new(),
            holding: false,
        }
    }

    /// Returns the text that should still be shown; anything that parsed as
    /// a call goes into `calls`.
    fn push(&mut self, text: &str, calls: &mut Vec<ToolCall>) -> String {
        self.buf.push_str(text);
        let mut out = String::new();
        loop {
            if !self.holding {
                match candidate_start(&self.buf) {
                    Some(at) => {
                        out.push_str(&self.buf[..at]);
                        self.buf.drain(..at);
                        self.holding = true;
                    }
                    None => {
                        // keep back only what could still grow into the start
                        // of a call. Holding a whole `{...}` on the chance it
                        // turns out to be one would stall the display of every
                        // code block until its braces balanced
                        let hold = tail_hold(&self.buf);
                        let emit = self.buf.len() - hold;
                        out.push_str(&self.buf[..emit]);
                        self.buf.drain(..emit);
                        break;
                    }
                }
            }
            match self.take(calls.len()) {
                Taken::Call(c) => {
                    calls.push(c);
                    self.holding = false;
                }
                Taken::Text(len) => {
                    out.push_str(&self.buf[..len]);
                    self.buf.drain(..len);
                    self.holding = false;
                }
                Taken::More => break,
            }
        }
        out
    }

    /// End of the stream: whatever is still held is either a call or text.
    fn flush(&mut self, calls: &mut Vec<ToolCall>) -> String {
        if self.holding
            && let Taken::Call(c) = self.take_final(calls.len())
        {
            calls.push(c);
            self.buf.clear();
        }
        self.holding = false;
        std::mem::take(&mut self.buf)
    }

    /// Tries to take a whole call off the front of the buffer.
    fn take(&mut self, seq: usize) -> Taken {
        if let Some(rest) = self.buf.strip_prefix(TOOL_OPEN) {
            let Some(end) = rest.find(TOOL_CLOSE) else {
                return Taken::More;
            };
            let inner = rest[..end].to_string();
            let span = TOOL_OPEN.len() + end + TOOL_CLOSE.len();
            return match call_from_json(&inner, &self.names, seq) {
                Some(c) => {
                    self.buf.drain(..span);
                    Taken::Call(c)
                }
                None => Taken::Text(span),
            };
        }
        match json_object_end(&self.buf) {
            None => Taken::More,
            Some(end) => match call_from_json(&self.buf[..end], &self.names, seq) {
                Some(c) => {
                    self.buf.drain(..end);
                    Taken::Call(c)
                }
                None => Taken::Text(end),
            },
        }
    }

    /// Same, for a span the stream ended in the middle of: an unclosed
    /// `<tool_call>` still holds a usable call.
    fn take_final(&mut self, seq: usize) -> Taken {
        let body = self
            .buf
            .strip_prefix(TOOL_OPEN)
            .unwrap_or(&self.buf)
            .trim_end_matches(TOOL_CLOSE);
        match call_from_json(body, &self.names, seq) {
            Some(c) => Taken::Call(c),
            None => Taken::Text(self.buf.len()),
        }
    }
}

/// Where a span that might be a call starts: an explicit tag, or a json
/// object whose first key is `name`. Requiring the key keeps ordinary prose
/// and code blocks flowing through without being held back.
fn candidate_start(s: &str) -> Option<usize> {
    let tag = s.find(TOOL_OPEN);
    let json = json_name_start(s);
    match (tag, json) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A confirmed `{` + `"name"`. Anything less certain is not held as a
/// candidate, only kept back by `tail_hold` until the next chunk says.
fn json_name_start(s: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = s[from..].find('{') {
        let at = from + rel;
        if s[at + 1..].trim_start().starts_with("\"name\"") {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// How many bytes at the end of the buffer could still turn into the start
/// of a call once more of the stream arrives: part of a `<tool_call>` tag,
/// or a trailing `{` that has not said what follows it yet. A handful of
/// bytes, never a whole object.
fn tail_hold(s: &str) -> usize {
    let tag = longest_suffix_prefix(s, TOOL_OPEN);
    let json = s
        .rfind('{')
        .map(|at| {
            let rest = &s[at + 1..];
            let trimmed = rest.trim_start();
            if rest.len() <= 8 && "\"name\"".starts_with(trimmed) {
                s.len() - at
            } else {
                0
            }
        })
        .unwrap_or(0);
    tag.max(json)
}

/// Index just past the `}` that closes the object the text starts with.
fn json_object_end(s: &str) -> Option<usize> {
    // callers only hand this a span that starts with '{'; saying so here
    // means a future one cannot walk the count below zero
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + c.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// `{"name": "grep", "arguments": {...}}` and its variants, but only for a
/// tool that exists: anything else is the model talking, not calling.
fn call_from_json(text: &str, names: &[String], seq: usize) -> Option<ToolCall> {
    let text = text
        .trim()
        .trim_start_matches("```json")
        .trim_matches('`')
        .trim();
    let v: Value = serde_json::from_str(text).ok()?;
    let name = v.get("name").or_else(|| v.get("tool"))?.as_str()?;
    if !names.iter().any(|n| n == name) {
        return None;
    }
    let args = v
        .get("arguments")
        .or_else(|| v.get("parameters"))
        .or_else(|| v.get("input"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Some(ToolCall {
        id: format!("text_call_{seq}"),
        kind: "function".into(),
        function: FunctionCall {
            name: name.to_string(),
            // some models put the arguments in a json string of their own
            arguments: match args {
                Value::String(s) => s,
                other => other.to_string(),
            },
        },
    })
}

fn longest_suffix_prefix(s: &str, tag: &str) -> usize {
    let max = (tag.len() - 1).min(s.len());
    for l in (1..=max).rev() {
        let start = s.len() - l;
        if s.is_char_boundary(start) && tag.as_bytes().starts_with(&s.as_bytes()[start..]) {
            return l;
        }
    }
    0
}

/// The two filters a text transport needs, and what they pull out of it.
/// Keeping them together is what stops the two transports from each growing
/// their own copy of this handling.
pub(super) struct TextStream {
    think: ThinkFilter,
    tools: ToolTextFilter,
    calls: Vec<ToolCall>,
}

impl TextStream {
    pub(super) fn new(tools: &Value) -> Self {
        Self {
            think: ThinkFilter::default(),
            tools: ToolTextFilter::new(tools),
            calls: Vec::new(),
        }
    }

    /// One piece of `content` off the wire.
    pub(super) fn content(
        &mut self,
        text: &str,
        turn: &mut AssistantTurn,
        on_event: &mut impl FnMut(StreamEvent),
    ) {
        for (is_reasoning, piece) in self.think.push(text) {
            self.emit(is_reasoning, piece, turn, on_event);
        }
    }

    /// End of the stream: whatever the filters still hold is content or a
    /// call, and a call read out of the text counts only when the model sent
    /// no proper ones.
    pub(super) fn finish(
        mut self,
        turn: &mut AssistantTurn,
        on_event: &mut impl FnMut(StreamEvent),
    ) {
        for (is_reasoning, piece) in self.think.flush() {
            self.emit(is_reasoning, piece, turn, on_event);
        }
        let shown = self.tools.flush(&mut self.calls);
        if !shown.is_empty() {
            turn.content.push_str(&shown);
            on_event(StreamEvent::Content(shown));
        }
        if turn.tool_calls.is_empty() {
            turn.tool_calls = self.calls;
        }
    }

    fn emit(
        &mut self,
        is_reasoning: bool,
        piece: String,
        turn: &mut AssistantTurn,
        on_event: &mut impl FnMut(StreamEvent),
    ) {
        if is_reasoning {
            on_event(StreamEvent::Reasoning(piece));
            return;
        }
        let shown = self.tools.push(&piece, &mut self.calls);
        if !shown.is_empty() {
            turn.content.push_str(&shown);
            on_event(StreamEvent::Content(shown));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> ToolTextFilter {
        ToolTextFilter::new(&json!([
            {"type": "function", "function": {"name": "read_file"}},
            {"type": "function", "function": {"name": "grep"}}
        ]))
    }

    /// Feeds the text in one piece and returns (shown, calls).
    fn run(text: &str) -> (String, Vec<ToolCall>) {
        run_chunked(&[text])
    }

    /// Feeds the text the way a stream would, in pieces.
    fn run_chunked(chunks: &[&str]) -> (String, Vec<ToolCall>) {
        let mut f = filter();
        let mut calls = Vec::new();
        let mut shown = String::new();
        for c in chunks {
            shown.push_str(&f.push(c, &mut calls));
        }
        shown.push_str(&f.flush(&mut calls));
        (shown, calls)
    }

    #[test]
    fn reads_a_tool_call_the_model_wrote_as_text() {
        let (shown, calls) = run(
            "sure\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"src/main.rs\"}}\n</tool_call>",
        );
        assert_eq!(calls.len(), 1, "{shown}");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"src/main.rs\"}");
        // the json never reaches the transcript
        assert_eq!(shown.trim(), "sure");
    }

    #[test]
    fn reads_a_bare_json_call_and_one_in_a_fence() {
        let (_, calls) = run("{\"name\": \"grep\", \"parameters\": {\"pattern\": \"fn main\"}}");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments, "{\"pattern\":\"fn main\"}");

        let (_, calls) =
            run("```json\n{\"name\": \"grep\", \"arguments\": {\"pattern\": \"x\"}}\n```");
        assert_eq!(calls.len(), 1, "a fenced call is still a call");
    }

    #[test]
    fn survives_being_split_across_chunks() {
        let (shown, calls) = run_chunked(&[
            "ok <tool_",
            "call>{\"name\": \"read_",
            "file\", \"argum",
            "ents\": {\"path\": \"a.rs\"}}</tool_call> done",
        ]);
        assert_eq!(calls.len(), 1, "{shown}");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(shown, "ok  done");
    }

    #[test]
    fn an_unclosed_tag_at_the_end_of_the_stream_still_counts() {
        let (_, calls) =
            run("<tool_call>{\"name\": \"grep\", \"arguments\": {\"pattern\": \"x\"}}");
        assert_eq!(calls.len(), 1);
    }

    /// Chunks land wherever the model's tokens land, often right on a `{`.
    /// The filter must not hold a whole code block back waiting to see
    /// whether it is a call: the text has to keep flowing.
    #[test]
    fn a_chunk_ending_on_a_brace_does_not_stall_the_stream() {
        let mut f = filter();
        let mut calls = Vec::new();
        let shown = f.push("here you go:\n\nfn main() {", &mut calls);
        assert!(
            shown.starts_with("here you go:"),
            "the text before the brace must go out at once: {shown:?}"
        );
        assert!(shown.contains("fn main()"), "{shown:?}");
        // at most the brace itself is held back
        assert!(
            f.buf.len() <= 1,
            "held back more than the brace: {:?}",
            f.buf
        );
        let rest = f.push("\n    let x = 1;\n}\n", &mut calls);
        let all = format!("{shown}{rest}{}", f.flush(&mut calls));
        assert_eq!(all, "here you go:\n\nfn main() {\n    let x = 1;\n}\n");
        assert!(calls.is_empty());
    }

    #[test]
    fn a_call_split_right_after_the_brace_is_still_read() {
        let (shown, calls) = run_chunked(&[
            "{",
            "\"name\": \"grep\", ",
            "\"arguments\": {\"pattern\": \"x\"}}",
        ]);
        assert_eq!(calls.len(), 1, "{shown:?}");
        assert_eq!(shown, "");
    }

    #[test]
    fn text_that_is_not_a_call_comes_back_untouched() {
        // a tool that does not exist is the model talking about json
        let same = "{\"name\": \"launch_missiles\", \"arguments\": {}}";
        let (shown, calls) = run(same);
        assert!(calls.is_empty());
        assert_eq!(shown, same);

        // prose, code and braces flow through
        for text in [
            "here is a struct:\n\n```rust\nstruct A { name: String }\n```\n",
            "use serde_json::json; let v = json!({\"a\": 1});",
            "{ this is not json at all }",
            "the file has {} in it",
        ] {
            let (shown, calls) = run(text);
            assert!(calls.is_empty(), "{text} became a call");
            assert_eq!(shown, text, "text changed");
        }
    }

    #[test]
    fn broken_json_is_left_alone() {
        let text = "<tool_call>{\"name\": \"read_file\", \"argu</tool_call>";
        let (shown, calls) = run(text);
        assert!(calls.is_empty());
        assert_eq!(shown, text, "a half-written call must still be readable");
    }

    #[test]
    fn several_calls_in_one_reply() {
        let (shown, calls) = run(
            "<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"a\"}}</tool_call>\
             <tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"b\"}}</tool_call>",
        );
        assert_eq!(calls.len(), 2, "{shown}");
        assert_ne!(calls[0].id, calls[1].id, "ids must be distinct");
    }

    #[test]
    fn arguments_that_arrive_as_a_json_string_are_kept_as_written() {
        let (_, calls) =
            run("{\"name\": \"grep\", \"arguments\": \"{\\\"pattern\\\": \\\"x\\\"}\"}");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments, "{\"pattern\": \"x\"}");
    }
}
