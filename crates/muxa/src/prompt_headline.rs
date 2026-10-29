//! One-line summaries of prompts that were not typed by a person.
//!
//! An agent's latest "prompt" is often machine input: Claude Code delivers a
//! finished background task as a `<task-notification>` block, and muxa
//! delivers a peer request as `[muxa:req_…] New Review request via … `. Shown
//! raw, a status row reads `<task-notification>` and nothing else. These
//! render them as what they are; any other text passes through unchanged.

use std::borrow::Cow;

/// The prompt as a status line would show it.
#[must_use]
pub fn prompt_headline(prompt: &str) -> Cow<'_, str> {
    let trimmed = prompt.trim_start();
    if let Some(body) = trimmed.strip_prefix("<task-notification>") {
        return Cow::Owned(task_notification(body));
    }
    if let Some(rest) = trimmed.strip_prefix("[muxa:req_") {
        if let Some(headline) = peer_request(rest) {
            return Cow::Owned(headline);
        }
    }
    if trimmed.starts_with("[muxa] Claimed;") {
        if let Some(record) = trimmed
            .split_once("--- peer request ---\n")
            .and_then(|(_, rest)| rest.split_once("\n--- end peer request ---"))
            .and_then(|(record, _)| serde_json::from_str::<serde_json::Value>(record).ok())
        {
            if let (Some(kind), Some(author)) =
                (record["kind"].as_str(), record["from"]["label"].as_str())
            {
                return Cow::Owned(format!(
                    "↳ {} request from {}",
                    one_line(kind),
                    one_line(author)
                ));
            }
        }
    }
    Cow::Borrowed(prompt)
}

fn tag<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = body.find(&open)? + open.len();
    let end = start + body[start..].find(&close)?;
    Some(body[start..end].trim())
}

fn task_notification(body: &str) -> String {
    let summary = tag(body, "summary").filter(|summary| !summary.is_empty());
    match (summary, tag(body, "status")) {
        (Some(summary), _) => format!("⟳ {}", one_line(summary)),
        (None, Some(status)) => format!("⟳ background task {status}"),
        (None, None) => "⟳ background task".to_owned(),
    }
}

/// `[muxa:req_ID] New Review request via muxa CLI representing who@pane (…)`
/// → `↳ Review request from who@pane`.
fn peer_request(rest: &str) -> Option<String> {
    let text = rest.split_once("] ")?.1;
    let line = text.lines().next()?;
    let kind = line
        .split(" via ")
        .next()?
        .trim()
        .trim_start_matches("New ")
        .trim();
    let from = line
        .split_once("representing ")
        .and_then(|(_, who)| who.split_whitespace().next())
        .map(|who| who.trim_end_matches([',', '.', ';']));
    Some(match from {
        Some(from) if !kind.is_empty() => format!("↳ {kind} from {from}"),
        _ if !kind.is_empty() => format!("↳ {kind}"),
        _ => "↳ peer request".to_owned(),
    })
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_notifications_read_as_their_summary() {
        let prompt = "<task-notification>\n<task-id>a0de</task-id>\n<status>completed</status>\n\
                      <summary>Agent \"Audit pane-duplicate handling\" finished</summary>\n</task-notification>";
        assert_eq!(
            prompt_headline(prompt),
            "⟳ Agent \"Audit pane-duplicate handling\" finished"
        );
        assert_eq!(
            prompt_headline("<task-notification>\n<status>failed</status>"),
            "⟳ background task failed"
        );
    }

    #[test]
    fn peer_requests_read_as_kind_and_sender() {
        let prompt = "[muxa:req_18daa6e47aa3da13_4] New Review request via muxa CLI representing \
                      implementer@rmux:%218 (caller %218, pid 4084076). Claim/read it with muxa_inbox";
        assert_eq!(
            prompt_headline(prompt),
            "↳ Review request from implementer@rmux:%218"
        );
    }

    #[test]
    fn typed_prompts_pass_through_untouched() {
        for prompt in [
            "좋습니다 바로 진행하세요",
            "<b>bold</b> please",
            "[draft] notes",
            "",
        ] {
            assert!(matches!(prompt_headline(prompt), Cow::Borrowed(p) if p == prompt));
        }
    }
}
