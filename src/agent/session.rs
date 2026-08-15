//! Per-project session state kept in `~/.thoth/projects/<encoded-cwd>/`:
//! the full transcript (so `thoth --continue` can pick up where the last run
//! stopped) and the permission allowlist ("always allow" survives a restart).

use crate::client::{Message, ToolCall};
use crate::tools::memory::project_dir;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/// Owned mirror of Message: its `role` is a &'static str, which cannot be
/// deserialized from a file.
#[derive(Serialize, Deserialize)]
struct Stored {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

fn session_path() -> PathBuf {
    project_dir().join("session.json")
}

fn allow_path() -> PathBuf {
    project_dir().join("allow.json")
}

/// Saves everything after the system message; the prompt is rebuilt on load
/// so a resumed session sees the current date, git branch and memory.
pub fn save(messages: &[Message]) {
    let stored: Vec<Stored> = messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| Stored {
            role: m.role.to_string(),
            content: m.content.clone(),
            tool_calls: m.tool_calls.clone(),
            tool_call_id: m.tool_call_id.clone(),
            name: m.name.clone(),
        })
        .collect();
    if stored.is_empty() {
        clear();
        return;
    }
    let Ok(json) = serde_json::to_string(&stored) else {
        return;
    };
    write_private(&session_path(), &json);
}

/// Writes through a temp file so a crash mid-write cannot leave a truncated
/// session behind, and keeps it readable by this user only.
fn write_private(path: &std::path::Path, content: &str) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, content).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // transcripts hold whatever the tools printed: keep them to the owner
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub fn load() -> Option<Vec<Message>> {
    let text = std::fs::read_to_string(session_path()).ok()?;
    let stored: Vec<Stored> = serde_json::from_str(&text).ok()?;
    let msgs: Vec<Message> = stored
        .into_iter()
        .filter_map(|s| {
            Some(Message {
                role: static_role(&s.role)?,
                content: s.content,
                tool_calls: s.tool_calls,
                tool_call_id: s.tool_call_id,
                name: s.name,
            })
        })
        .collect();
    let msgs = repair(msgs);
    if msgs.is_empty() { None } else { Some(msgs) }
}

/// A transcript that stops between a tool call and its result is rejected by
/// every api that checks: "an assistant message with tool_calls must be
/// followed by tool messages". A run killed mid-turn can leave one behind,
/// and the user would have no way past it but `/clear`. Half a turn is
/// dropped rather than resumed.
fn repair(mut msgs: Vec<Message>) -> Vec<Message> {
    let answered: HashSet<String> = msgs
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    for m in msgs.iter_mut() {
        if let Some(calls) = &m.tool_calls
            && !calls.iter().all(|c| answered.contains(&c.id))
        {
            m.tool_calls = None;
        }
    }
    // and the other way round: a result whose call is gone is just as bad
    let called: HashSet<String> = msgs
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|c| c.id.clone())
        .collect();
    msgs.retain(|m| {
        m.role != "tool"
            || m.tool_call_id
                .as_ref()
                .is_some_and(|id| called.contains(id))
    });
    msgs.retain(|m| m.content.is_some() || m.tool_calls.is_some());
    msgs
}

pub fn clear() {
    let _ = std::fs::remove_file(session_path());
}

fn static_role(role: &str) -> Option<&'static str> {
    match role {
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        "tool" => Some("tool"),
        "system" => Some("system"),
        _ => None,
    }
}

pub fn load_allow() -> HashSet<String> {
    std::fs::read_to_string(allow_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_allow(tools: &HashSet<String>) {
    if let Ok(json) = serde_json::to_string(tools) {
        write_private(&allow_path(), &json);
    }
}

pub fn clear_allow() {
    let _ = std::fs::remove_file(allow_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_transcript() {
        let msgs = [
            Message::system("prompt"),
            Message::user("hello"),
            Message::tool("call_1".into(), "grep".into(), "result".into()),
        ];
        let stored: Vec<Stored> = msgs
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| Stored {
                role: m.role.to_string(),
                content: m.content.clone(),
                tool_calls: m.tool_calls.clone(),
                tool_call_id: m.tool_call_id.clone(),
                name: m.name.clone(),
            })
            .collect();
        let json = serde_json::to_string(&stored).unwrap();
        let back: Vec<Stored> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2, "the system message is not stored");
        assert_eq!(static_role(&back[0].role), Some("user"));
        assert_eq!(back[1].name.as_deref(), Some("grep"));
        assert_eq!(back[1].tool_call_id.as_deref(), Some("call_1"));
    }

    /// What a run killed between the call and its result leaves behind.
    /// Resuming that transcript is a 400 from every api that checks.
    #[test]
    fn half_a_turn_is_not_resumed() {
        let call = |id: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: crate::client::FunctionCall {
                name: "grep".into(),
                arguments: "{}".into(),
            },
        };
        let msgs = vec![
            Message::user("find it"),
            Message::assistant(Some("looking".into()), Some(vec![call("a")])),
            Message::tool("a".into(), "grep".into(), "found".into()),
            // the run died here, after the call went out and before the result
            Message::assistant(None, Some(vec![call("b")])),
        ];
        let out = repair(msgs);
        assert_eq!(out.len(), 3, "the unanswered call must go");
        assert!(out[1].tool_calls.is_some(), "the answered one stays");

        // and a result with nothing that asked for it
        let orphan = vec![
            Message::user("hello"),
            Message::tool("gone".into(), "grep".into(), "result".into()),
        ];
        assert_eq!(repair(orphan).len(), 1);
    }

    #[test]
    fn rejects_unknown_roles() {
        assert_eq!(static_role("developer"), None);
    }
}
