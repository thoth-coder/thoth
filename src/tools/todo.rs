//! The plan for the task at hand, kept where both the model and the user can
//! see it. Small models lose track of a five step job around step three; a
//! list they rewrite as they go is state they no longer have to hold in their
//! head, and it lets the user see the plan is wrong before the work is done.

use anyhow::{Result, bail};
use serde::Deserialize;

/// Enough for a real task, few enough that the list cannot become the
/// context. A job with more steps than this wants splitting anyway.
const MAX_ITEMS: usize = 12;
const MAX_TEXT: usize = 100;

#[derive(Default, PartialEq, Clone, Copy)]
pub enum Status {
    #[default]
    Todo,
    Doing,
    Done,
}

/// Read loosely on purpose. A model that writes "in_progress" or "Pending"
/// means something obvious, and failing the whole call over the spelling of
/// one word costs a round trip to learn nothing.
impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        parse_status(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!("unknown status '{raw}': use todo, doing or done"))
        })
    }
}

fn parse_status(raw: &str) -> Option<Status> {
    let s = raw.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    Some(match s.as_str() {
        "done" | "complete" | "completed" | "finished" | "x" => Status::Done,
        "doing" | "in_progress" | "active" | "current" | "started" | "working" => Status::Doing,
        "todo" | "to_do" | "pending" | "open" | "not_started" | "" => Status::Todo,
        _ => return None,
    })
}

#[derive(Deserialize)]
pub struct Item {
    pub text: String,
    #[serde(default)]
    pub status: Status,
}

#[derive(Deserialize)]
pub struct TodoArgs {
    pub items: Vec<Item>,
}

pub fn write(a: TodoArgs) -> Result<String> {
    if a.items.is_empty() {
        bail!("items is empty: send the whole list every time, not just the change");
    }
    if a.items.len() > MAX_ITEMS {
        bail!(
            "{} items is too many for one plan (max {MAX_ITEMS}). group them, or split the task",
            a.items.len()
        );
    }
    Ok(render(&a.items))
}

fn render(items: &[Item]) -> String {
    let mut out = String::new();
    for i in items {
        let mark = match i.status {
            Status::Done => "x",
            Status::Doing => ">",
            Status::Todo => " ",
        };
        let text = i.text.trim();
        let text: String = if text.chars().count() > MAX_TEXT {
            text.chars().take(MAX_TEXT).collect::<String>() + "…"
        } else {
            text.to_string()
        };
        out.push_str(&format!("[{mark}] {text}\n"));
    }
    let left = items.iter().filter(|i| i.status != Status::Done).count();
    out.push_str(&match left {
        0 => "all done".to_string(),
        n => format!("{n} left"),
    });
    out
}

/// Short form for the tool header in the interface.
pub fn summary(args: &serde_json::Value) -> String {
    let items = args.get("items").and_then(|v| v.as_array());
    let Some(items) = items else {
        return String::new();
    };
    // read the same way the tool itself reads it, or a model that writes
    // "completed" gets a header saying nothing is done
    let status = |i: &serde_json::Value| {
        i.get("status")
            .and_then(|s| s.as_str())
            .and_then(parse_status)
            .unwrap_or_default()
    };
    let done = items.iter().filter(|i| status(i) == Status::Done).count();
    // the one being worked on is the useful half of the header
    let doing = items
        .iter()
        .find(|i| status(i) == Status::Doing)
        .and_then(|i| i.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    format!("{done}/{} done  {doing}", items.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn items(v: serde_json::Value) -> TodoArgs {
        serde_json::from_value(json!({ "items": v })).unwrap()
    }

    #[test]
    fn renders_a_plan_the_user_can_read() {
        let out = write(items(json!([
            {"text": "read the config", "status": "done"},
            {"text": "add the field", "status": "doing"},
            {"text": "update the docs"}
        ])))
        .unwrap();
        assert_eq!(
            out,
            "[x] read the config\n[>] add the field\n[ ] update the docs\n2 left"
        );
    }

    #[test]
    fn says_when_it_is_finished() {
        let out = write(items(json!([{"text": "one", "status": "done"}]))).unwrap();
        assert!(out.ends_with("all done"), "{out}");
    }

    #[test]
    fn refuses_a_plan_that_would_become_the_context() {
        let many: Vec<serde_json::Value> =
            (0..20).map(|i| json!({"text": i.to_string()})).collect();
        assert!(write(items(json!(many))).is_err());
        assert!(write(items(json!([]))).is_err());
    }

    #[test]
    fn a_long_step_is_cut_not_wrapped() {
        let out = write(items(json!([{"text": "x".repeat(300)}]))).unwrap();
        assert!(out.lines().next().unwrap().chars().count() < 110, "{out}");
    }

    /// What the schema asks for is one wording of three; models write the
    /// others. Losing a turn to that is the kind of tax a small model
    /// cannot afford.
    #[test]
    fn a_status_written_another_way_still_reads() {
        let out = write(items(json!([
            {"text": "one", "status": "Completed"},
            {"text": "two", "status": "in_progress"},
            {"text": "three", "status": "pending"}
        ])))
        .unwrap();
        assert_eq!(out, "[x] one\n[>] two\n[ ] three\n2 left");
        // but a word that means nothing here is still an error, not a guess
        assert!(
            serde_json::from_value::<TodoArgs>(json!({"items": [
                {"text": "one", "status": "blocked"}
            ]}))
            .is_err()
        );
    }

    #[test]
    fn the_header_shows_progress_and_what_is_running() {
        let s = summary(&json!({"items": [
            {"text": "a", "status": "done"},
            {"text": "run the tests", "status": "doing"}
        ]}));
        assert_eq!(s, "1/2 done  run the tests");
        let s = summary(&json!({"items": [
            {"text": "a", "status": "Completed"},
            {"text": "run the tests", "status": "in-progress"}
        ]}));
        assert_eq!(s, "1/2 done  run the tests");
    }
}
