//! Making room in the conversation.
//!
//! Context is the scarce resource, and three different things spend it: the
//! summary that replaces a history too long to send, the trim that saves a
//! request the server has already truncated, and the older copy of a result
//! that a newer identical read has superseded. All three answer the same
//! question — what leaves the conversation — so they live together, away from
//! the loop that fills it.
//!
//! A child module of `agent`, so these stay `impl Agent` and keep reading the
//! private fields they have always read.

use super::*;

impl Agent {
    /// Everything that remembers what is in the context, dropped together
    /// when the context itself is dropped.
    pub(super) fn forget_call_history(&mut self) {
        self.repeated.clear();
        self.call_counts.clear();
    }

    pub(super) async fn compact(&mut self) {
        if self.messages.len() <= 1 {
            self.send(AgentEvent::Info("nothing to compact yet".into()));
            return;
        }
        let token = CancellationToken::new();
        *self.cancel_slot.lock().unwrap() = token.clone();
        // trim bulky tool outputs in the copy we summarize, so the summary
        // request itself fits even when the window is nearly full
        let mut msgs = self.messages.clone();
        for m in msgs.iter_mut() {
            if m.role == "tool"
                && let Some(c) = &m.content
                && c.chars().count() > 400
            {
                let cut: String = c.chars().take(400).collect();
                m.content = Some(format!("{cut}\n...(trimmed)"));
            }
        }
        msgs.push(Message::user(
            "Summarize this entire conversation so it can be continued later: the user's \
             original request, key facts and decisions, files read or changed (with paths), \
             current state, and what remains to be done. Write the summary in the language the \
             user used, so the conversation continues in it. Reply with only the summary.",
        ));
        let before = self.messages.len();
        match self
            .client
            .chat_stream(&msgs, &serde_json::json!([]), token, |_| {})
            .await
        {
            Ok(turn) if !turn.interrupted && !turn.content.trim().is_empty() => {
                let summary = turn.content.trim().to_string();
                tools::memory::save_recap(&summary);
                self.messages.truncate(1);
                self.forget_call_history();
                let mut carried =
                    format!("(conversation continued from a compacted summary)\n{summary}");
                if let Some(lang) = self.reply_language {
                    carried.push_str(&Self::language_note(lang));
                }
                self.messages.push(Message::user(carried));
                self.send(AgentEvent::Info(format!(
                    "context compacted: {before} messages -> 2. recap saved, /recap restores it next session"
                )));
            }
            Ok(turn) if turn.interrupted => {
                self.send(AgentEvent::Info("compact cancelled".into()));
            }
            Ok(_) => {
                // summary came back empty/truncated — free space mechanically
                self.hard_trim();
            }
            Err(e) => {
                self.send(AgentEvent::Error(format!("compact failed: {e:#}")));
                self.hard_trim();
            }
        }
    }

    /// Model-free fallback: shorten old tool results in place, keeping the
    /// last few intact.
    pub(super) fn hard_trim(&mut self) {
        // a result cut to 200 characters is a result the model has to read
        // again, so the count of what it already read goes with it
        self.forget_call_history();
        let keep_from = self.messages.len().saturating_sub(4);
        let mut trimmed = 0usize;
        for m in self.messages.iter_mut().take(keep_from) {
            if m.role != "tool" {
                continue;
            }
            if let Some(c) = &m.content
                && c.chars().count() > 200
            {
                let cut: String = c.chars().take(200).collect();
                m.content = Some(format!("{cut}\n...(trimmed to save context)"));
                trimmed += 1;
            }
        }
        self.send(AgentEvent::Info(format!(
            "trimmed {trimmed} old tool results to free context"
        )));
    }

    /// Calls whose result is the same every time, so a second one is a copy
    /// of the first and a third is a model going in circles.
    fn is_read_only(name: &str) -> bool {
        matches!(
            name,
            "read_file" | "grep" | "glob" | "list_dir" | "problems" | "web_fetch" | "web_search"
        )
    }

    /// A file that just changed makes every earlier read of it out of date.
    /// An identical read_file after an edit is not a repeat: it is the only
    /// way to see what is there now, and it is exactly what a model does
    /// after its own edit came out wrong. The breaker counts calls, not
    /// results, so without this it blocks the read that would let the model
    /// recover, and tells it "the result will not change" while the file on
    /// disk says otherwise.
    pub(super) fn forget_reads_of(&mut self, tc: &ToolCall) {
        if !matches!(
            tc.function.name.as_str(),
            "write_file" | "edit_file" | "multi_edit" | "move_file" | "delete_file"
        ) {
            return;
        }
        let Ok(args) = serde_json::from_str::<Value>(&tc.function.arguments) else {
            return;
        };
        for key in ["path", "from", "to"] {
            let Some(p) = args.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            // the write may name the file relatively and the read
            // absolutely: match on the name, where over-matching costs one
            // more allowed read of a file that happens to share it
            let name = std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string());
            // only the count, never `repeated`. That map is what lets the
            // re-read supersede the copy taken before the change, and after
            // a change that copy is not merely old, it is wrong: dropping
            // the entry would leave both versions of the file in the
            // context for the model to choose between
            self.call_counts.retain(|k, _| {
                !k.split_once(':')
                    .is_some_and(|(tool, args)| Self::is_read_only(tool) && args.contains(&name))
            });
        }
        // and every command, which name no file but read the whole tree.
        // `cargo test` after a fix is the same command and a different
        // answer, and blocking it is blocking the one habit worth having
        self.call_counts.retain(|k, _| !k.starts_with("shell:"));
    }

    /// A model that reads the same thing twice leaves two copies of it in the
    /// context, and the older one is dead weight. When a read-only call is
    /// repeated with the exact same arguments, the earlier result is replaced
    /// by a one-line note. Identical arguments is the whole safety condition:
    /// a read of another range, or a search with another pattern, says
    /// something the newer one does not, and is left alone. The todo list is
    /// the exception that needs no arguments to match: it is one list.
    pub(super) fn drop_stale_copy(&mut self, tc: &ToolCall) {
        let name = tc.function.name.as_str();
        let key = if name == "todo" {
            // the plan is one thing that keeps being rewritten: only the
            // latest version of it means anything
            "todo".to_string()
        } else if Self::is_read_only(name) {
            format!("{name}:{}", tc.function.arguments)
        } else {
            return;
        };
        let Some(old_id) = self.repeated.insert(key, tc.id.clone()) else {
            return;
        };
        if old_id == tc.id {
            return;
        }
        if let Some(m) = self
            .messages
            .iter_mut()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(old_id.as_str()))
        {
            m.content = Some("(the same call was made again later; see that result)".into());
        }
    }
}
