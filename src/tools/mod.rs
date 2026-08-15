pub mod fs;
pub mod memory;
pub mod search;
pub mod shell;
pub mod todo;
pub mod web;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// How many characters of a tool result are worth keeping, for a context
/// window of this many tokens. One result should not crowd out the
/// conversation, but a large window is allowed to use the room it has: the
/// old fixed 12k was tuned for 16k windows and made a 200k one useless.
pub fn output_cap(window: Option<u32>) -> usize {
    match window {
        Some(w) => (w as usize * 3 / 4).clamp(4_000, 80_000),
        // nobody said how big the window is: keep the number that was tuned
        // for 16k, because guessing high is how a small server gets its
        // context eaten by one grep
        None => 12_000,
    }
}

/// `with_problems` drops the editor tool when no editor is connected: a tool
/// that cannot work is schema tokens spent on a wrong turn waiting to happen.
pub fn definitions(with_problems: bool) -> Value {
    let mut defs = json!([
        {"type": "function", "function": {
            "name": "read_file",
            "description": "Read a text file from disk. Returns the content with line numbers. For large files use offset (1-based start line) and limit (max lines).",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                "offset": {"type": "integer", "description": "1-based line number to start reading from"},
                "limit": {"type": "integer", "description": "Maximum number of lines to return"}
            }, "required": ["path"]}
        }},
        {"type": "function", "function": {
            "name": "write_file",
            "description": "Create or overwrite a file with the given content. Parent directories are created automatically. For small changes to existing files prefer edit_file.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "description": "Full content to write"}
            }, "required": ["path", "content"]}
        }},
        {"type": "function", "function": {
            "name": "edit_file",
            "description": "Replace an exact text snippet in an existing file. old_string must match the file content exactly (including whitespace and indentation) and must be unique in the file unless replace_all is true. Read the file first to get the exact text. Never include the line-number prefix from read_file output in old_string or new_string.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact existing text to find"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            }, "required": ["path", "old_string", "new_string"]}
        }},
        {"type": "function", "function": {
            "name": "multi_edit",
            "description": "Several exact replacements in ONE file, in order, all of them or none. Same rules as edit_file. Use this instead of calling edit_file twice on the same file.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"},
                "edits": {"type": "array", "description": "In order; a later edit sees what the earlier ones did", "items": {"type": "object", "properties": {
                    "old_string": {"type": "string", "description": "Exact existing text to find"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
                }, "required": ["old_string", "new_string"]}}
            }, "required": ["path", "edits"]}
        }},
        {"type": "function", "function": {
            "name": "move_file",
            "description": "Rename or move a file. The destination must not exist.",
            "parameters": {"type": "object", "properties": {
                "from": {"type": "string"},
                "to": {"type": "string"}
            }, "required": ["from", "to"]}
        }},
        {"type": "function", "function": {
            "name": "delete_file",
            "description": "Delete one file inside the working directory. You must have read the whole file first. Never use it to get around the read-before-write rule.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"}
            }, "required": ["path"]}
        }},
        {"type": "function", "function": {
            "name": "todo",
            "description": "The plan for the task you are on. Send the whole list every time, exactly one item marked doing. Use it for anything with more than about three steps, before starting.",
            "parameters": {"type": "object", "properties": {
                "items": {"type": "array", "items": {"type": "object", "properties": {
                    "text": {"type": "string"},
                    "status": {"type": "string", "enum": ["todo", "doing", "done"]}
                }, "required": ["text"]}}
            }, "required": ["items"]}
        }},
        {"type": "function", "function": {
            "name": "list_dir",
            "description": "List the files and subdirectories in a directory.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Directory path (default: working directory)"}
            }}
        }},
        {"type": "function", "function": {
            "name": "glob",
            "description": "Find files by name pattern, e.g. '**/*.rs', 'src/**/*.ts', '*.toml'. Returns matching file paths.",
            "parameters": {"type": "object", "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Root directory to search (default: working directory)"}
            }, "required": ["pattern"]}
        }},
        {"type": "function", "function": {
            "name": "grep",
            "description": "Search file contents with a case-sensitive regular expression. Returns matching lines as path:line: text. With context set, also shows the surrounding lines, which is often enough to understand the code without reading the file.",
            "parameters": {"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for"},
                "path": {"type": "string", "description": "File or directory to search (default: working directory)"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. '*.rs'"},
                "context": {"type": "integer", "description": "Lines of surrounding code to show before and after each match (like grep -C). 4-6 works well when exploring. Max 10."}
            }, "required": ["pattern"]}
        }},
        {"type": "function", "function": {
            "name": "remember",
            "description": "Save one durable fact to project memory (persists across sessions and /clear). Use for: project conventions discovered the hard way, decisions the user made, gotchas that cost time. One short sentence per call. Do NOT save things already visible in the code.",
            "parameters": {"type": "object", "properties": {
                "fact": {"type": "string", "description": "One short sentence to remember"}
            }, "required": ["fact"]}
        }},
        {"type": "function", "function": {
            "name": "web_search",
            "description": "Search the web. Returns top results with title, URL and snippet. Use for library docs, current versions, unfamiliar errors, or anything not certain from memory. Follow up with web_fetch to read a page.",
            "parameters": {"type": "object", "properties": {
                "query": {"type": "string"}
            }, "required": ["query"]}
        }},
        {"type": "function", "function": {
            "name": "web_fetch",
            "description": "Fetch a URL and return its readable text content (HTML is converted to plain text).",
            "parameters": {"type": "object", "properties": {
                "url": {"type": "string", "description": "http(s) URL to fetch"}
            }, "required": ["url"]}
        }},
        {"type": "function", "function": {
            "name": "shell",
            "description": shell_description(),
            "parameters": {"type": "object", "properties": {
                "command": {"type": "string"},
                "background": {"type": "boolean", "description": "Run detached and return immediately with a pid and a log-file path. REQUIRED for servers and watch modes, because a foreground server blocks until timeout. Stop it later by killing the pid."},
                "timeout_secs": {"type": "integer", "description": "Foreground timeout in seconds (default 120, max 600). Raise it for slow builds."}
            }, "required": ["command"]}
        }}
    ]);
    if with_problems && let Some(list) = defs.as_array_mut() {
        list.push(json!({"type": "function", "function": {
            "name": "problems",
            "description": "Get the current errors and warnings from the user's editor (the IDE Problems panel, live from the language server). Cheap and fast. Use it after editing files to check whether problems are fixed, before falling back to a full build. Diagnostics may lag a moment behind edits.",
            "parameters": {"type": "object", "properties": {}}
        }}));
    }
    defs
}

fn shell_description() -> String {
    let base = "Run a shell command in the working directory and return its combined output. \
Use for builds, tests, git, package managers, and anything the other tools do not cover. \
The command already runs in the working directory, no need to cd first.";
    if cfg!(windows) {
        format!(
            "{base} The shell is Windows PowerShell 5.1: chain commands with ';' because '&&' is NOT supported."
        )
    } else {
        format!("{base} The shell is sh.")
    }
}

pub fn needs_permission(name: &str, args: &Value) -> bool {
    match name {
        "write_file" | "edit_file" | "multi_edit" | "move_file" | "delete_file" | "shell"
        | "remember" | "web_fetch" => true,
        // reading inside the project is free; reaching outside it is not
        "read_file" => !fs::inside_project(args.get("path").and_then(|v| v.as_str()).unwrap_or("")),
        _ => false,
    }
}

/// What an "always allow" answer covers. Granting every future shell command
/// from one prompt is too much, so shell is keyed by its program (`shell:git`)
/// and web_fetch by host; the rest are keyed by tool name.
pub fn permission_key(name: &str, args: &Value) -> String {
    let get = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "shell" => format!("shell:{}", program_of(get("command"))),
        "web_fetch" => format!("web_fetch:{}", host_of(get("url"))),
        _ => name.to_string(),
    }
}

/// Human wording for what an "always" answer will allow.
pub fn permission_scope(key: &str) -> String {
    match key.split_once(':') {
        Some(("shell", prog)) => format!("`{prog}` commands"),
        Some(("web_fetch", host)) => format!("fetching from {host}"),
        _ => format!("the {key} tool"),
    }
}

/// First word of a command line, without its path or extension:
/// "C:/bin/git.exe status" -> "git".
fn program_of(command: &str) -> String {
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first)
        .trim_end_matches(".exe")
        .trim_matches(['"', '\'']);
    if base.is_empty() {
        "?".into()
    } else {
        base.to_lowercase()
    }
}

fn host_of(url: &str) -> String {
    let rest = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if host.is_empty() {
        "?".into()
    } else {
        host.to_lowercase()
    }
}

/// Short one-line description of a call, for the tool header in the UI.
pub fn summarize(name: &str, args: &Value) -> String {
    let get = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let s = match name {
        "read_file" | "write_file" | "edit_file" => get("path").to_string(),
        "multi_edit" => format!(
            "{} ({} edits)",
            get("path"),
            args.get("edits")
                .and_then(|e| e.as_array())
                .map(|e| e.len())
                .unwrap_or(0)
        ),
        "todo" => todo::summary(args),
        "move_file" => format!("{} -> {}", get("from"), get("to")),
        "delete_file" => get("path").to_string(),
        "list_dir" => {
            let p = get("path");
            if p.is_empty() {
                ".".into()
            } else {
                p.to_string()
            }
        }
        "glob" | "grep" => get("pattern").to_string(),
        "remember" => get("fact").to_string(),
        "web_search" => get("query").to_string(),
        "web_fetch" => get("url").to_string(),
        "shell" => get("command").to_string(),
        _ => String::new(),
    };
    truncate_line(&s, 80)
}

/// Multi-line preview shown in the permission dialog.
pub fn preview(name: &str, args: &Value) -> String {
    match name {
        "shell" => {
            let bg = args
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            format!(
                "$ {}{}",
                args.get("command").and_then(|v| v.as_str()).unwrap_or("?"),
                if bg { "  (background)" } else { "" }
            )
        }
        "write_file" => fs::preview_write(args),
        "edit_file" => fs::preview_edit(args),
        "multi_edit" => fs::preview_multi_edit(args),
        "delete_file" => fs::preview_delete(args),
        "move_file" => format!(
            "Move {} to {}",
            args.get("from").and_then(|v| v.as_str()).unwrap_or("?"),
            args.get("to").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "read_file" => format!(
            "read {} (outside this project)",
            args.get("path").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "web_fetch" => format!(
            "fetch {}",
            args.get("url").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "remember" => format!(
            "save to project memory: {}",
            args.get("fact").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        _ => String::new(),
    }
}

pub async fn execute(
    name: &str,
    args: Value,
    cancel: CancellationToken,
    cap: usize,
) -> Result<String> {
    let out = match name {
        "read_file" => fs::read_file(serde_json::from_value(args)?, cap)?,
        "write_file" => fs::write_file(serde_json::from_value(args)?)?,
        "edit_file" => fs::edit_file(serde_json::from_value(args)?)?,
        "multi_edit" => fs::multi_edit(serde_json::from_value(args)?)?,
        "move_file" => fs::move_file(serde_json::from_value(args)?)?,
        "delete_file" => fs::delete_file(serde_json::from_value(args)?)?,
        "todo" => todo::write(serde_json::from_value(args)?)?,
        "list_dir" => fs::list_dir(serde_json::from_value(args)?, cap)?,
        "glob" => fs::glob_files(serde_json::from_value(args)?, cap)?,
        "grep" => search::grep(serde_json::from_value(args)?, cap)?,
        "problems" => crate::editor::diagnostics_report(),
        "remember" => memory::remember(serde_json::from_value(args)?)?,
        "web_search" => web::search(serde_json::from_value(args)?).await?,
        "web_fetch" => web::fetch(serde_json::from_value(args)?, cap).await?,
        "shell" => shell::run(serde_json::from_value(args)?, cancel).await?,
        _ => bail!("unknown tool: {name}"),
    };
    Ok(truncate_output(out, cap))
}

pub fn truncate_line(s: &str, max: usize) -> String {
    let s = s.lines().next().unwrap_or("");
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

pub fn truncate_output(s: String, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s;
    }
    let cut: String = s.chars().take(cap).collect();
    format!("{cut}\n… (output truncated at {cap} characters. narrow the search, or read a range)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_cap_follows_the_window() {
        // what a 16k window used to get, kept as the reference point
        assert_eq!(output_cap(Some(16_384)), 12_288);
        // a big window is allowed to use its room, a tiny one is protected
        assert!(output_cap(Some(200_000)) > output_cap(Some(32_768)));
        assert_eq!(output_cap(Some(200_000)), 80_000);
        assert_eq!(output_cap(Some(2_000)), 4_000);
        // an unknown window must not be treated as a generous one
        assert_eq!(output_cap(None), 12_000);
        assert!(output_cap(None) < output_cap(Some(32_768)));
    }

    #[test]
    fn the_editor_tool_is_only_offered_with_an_editor() {
        let names = |with: bool| {
            definitions(with)
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(names(true).contains(&"problems".to_string()));
        assert!(!names(false).contains(&"problems".to_string()));
        assert_eq!(names(true).len(), names(false).len() + 1);
    }

    #[test]
    fn truncation_says_where_the_limit_came_from() {
        let out = truncate_output("x".repeat(100), 20);
        assert!(out.starts_with(&"x".repeat(20)));
        assert!(out.contains("truncated at 20 characters"), "{out}");
        assert_eq!(truncate_output("short".into(), 20), "short");
    }
}
