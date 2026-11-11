//! Does a finished turn's response ask the person something?
//!
//! An agent that ends its turn by *writing* a question is `Idle` to every
//! hook: nothing tells tend it is blocked on a person. This module reads the
//! response the `Stop` hook already captured and answers with the sentence
//! that asked, or `None`. Deterministic: phrase and punctuation checks only,
//! no model call. See `docs/AWAITING_REPLY.md`.
//!
//! Three strengths of cue:
//! - **Decision requests** ("결정해 주실 것", "Decision needed") name a
//!   decision outright and count anywhere in the response: agents often put
//!   the decision in the middle and trail with notes.
//! - **Requests** ("할까요", "해도 될까요", "should I", "do you want me to")
//!   ask the person to act and count in the closing paragraphs, even with a
//!   status note after them.
//! - **A bare question mark** counts only on the response's last sentence, or
//!   when all that follows is a conditional on the answer ("If so, I'll…",
//!   "괜찮으시면 …"): "Why did CI fail? The key was missing." asks nothing.
//!
//! Courtesy closers and offers that do not block work ("더 필요한 것 있으면
//! 말씀해 주세요", "let me know if you have any questions", "Would you like a
//! walkthrough?") never count, nor does a question inside quotes or code.

/// How many trailing paragraphs a plain question may sit in.
const CLOSING_PARAGRAPHS: usize = 3;
/// Longest question text kept for display.
const MAX_QUESTION_CHARS: usize = 200;

/// Phrases that name a decision for the person, wherever they appear.
const DECISION_CUES: &[&str] = &[
    "결정해 주실 것",
    "결정해 주실 부분",
    "결정이 필요",
    "결정 필요",
    "결정해 주세요",
    "결정해 주시면",
    "정해 주세요",
    "정해 주시면",
    "선택해 주세요",
    "선택해 주시면",
    "골라 주세요",
    "골라 주시면",
    "승인해 주세요",
    "승인해 주시면",
    "승인이 필요",
    "decision needed",
    "needs your decision",
    "need your decision",
    "need your approval",
    "needs your approval",
    "need your go-ahead",
    "your call",
    "please decide",
    "please choose",
    "please confirm",
    "can you confirm",
];

/// Phrases that ask the person to act; only in the closing paragraphs.
const REQUEST_CUES: &[&str] = &[
    "할까요",
    "될까요",
    "괜찮을까요",
    "괜찮으신가요",
    "어떻게 할지",
    "알려 주시면",
    "말씀해 주시면",
    "확인해 주시면",
    "should i ",
    "shall i ",
    "do you want",
    "which one",
    "which option",
    "let me know which",
    "let me know whether",
    "let me know if you want",
    "let me know if you'd like",
];

/// Closers that read like a question but ask nothing. Matched against the
/// whole sentence (lowercased) as a substring.
const COURTESY: &[&str] = &[
    "필요한 것 있으면",
    "필요한 게 있으면",
    "필요하시면 말씀",
    "궁금한 점 있으면",
    "궁금한 점이 있으면",
    "궁금하신 점",
    "다른 도움",
    "더 도와드릴",
    "any questions",
    "anything else",
    "if you need anything",
    "if you have questions",
    "happy to help",
];

/// Openings of an offer: the work is done and this asks nothing that
/// blocks it.
const OFFER_OPENINGS: &[&str] = &[
    "would you like",
    "want me to",
    "원하시면",
    "필요하시면",
    "궁금하시면",
];

/// Openings of a sentence that only says what happens once the person
/// answers; it may follow the question without hiding it.
const FOLLOW_UPS: &[&str] = &[
    "if so",
    "if yes",
    "if you agree",
    "if that works",
    "if not",
    "once you",
    "otherwise",
    "괜찮으시면",
    "좋으시면",
    "그렇다면",
    "그러면",
    "동의하시면",
    "아니면",
];

/// The sentence in `response` that asks the person something, trimmed for
/// display, or `None` when the response asks nothing.
#[must_use]
pub fn classify(response: &str) -> Option<String> {
    let paragraphs = paragraphs(response);
    if paragraphs.is_empty() {
        return None;
    }
    let closing_start = paragraphs.len().saturating_sub(CLOSING_PARAGRAPHS);
    let sentences: Vec<(bool, String)> = paragraphs
        .iter()
        .enumerate()
        .flat_map(|(index, paragraph)| {
            sentences(paragraph)
                .into_iter()
                .map(move |sentence| (index >= closing_start, sentence))
        })
        .collect();
    let mut found: Option<&str> = None;
    for (position, (closing, sentence)) in sentences.iter().enumerate() {
        let lower = sentence.to_lowercase();
        let decision = DECISION_CUES.iter().any(|c| lower.contains(c));
        if !decision
            && (COURTESY.iter().any(|c| lower.contains(c))
                || OFFER_OPENINGS.iter().any(|c| lower.starts_with(c)))
        {
            continue;
        }
        let request = *closing && REQUEST_CUES.iter().any(|c| lower.contains(c));
        let last_question = (sentence.ends_with('?') || sentence.ends_with('？'))
            && sentences[position + 1..]
                .iter()
                .all(|(_, after)| is_follow_up(after));
        if decision || request || last_question {
            // The last asking sentence wins: it is the one the turn closes
            // on, or the latest decision named.
            found = Some(sentence);
        }
    }
    found.map(truncate)
}

fn is_follow_up(sentence: &str) -> bool {
    let lower = sentence.to_lowercase();
    FOLLOW_UPS.iter().any(|f| lower.starts_with(f))
}

/// Prose paragraphs, without fenced code, block quotes, rules or tables, and
/// with inline code removed (a `?` in code asks nothing).
fn paragraphs(response: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;
    for raw in response.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let structural = line.starts_with('>')
            || line.starts_with('|')
            || (!line.is_empty()
                && line
                    .chars()
                    .all(|c| matches!(c, '-' | '*' | '_' | '=' | ' ')))
            || line.starts_with('※')
            || line.contains("(disable recaps in /config)");
        if line.is_empty() || structural {
            if !current.trim().is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.clear();
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(&strip_inline_code(line));
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Removes inline code and double-quoted text: a question in either is
/// quoted, not asked.
fn strip_inline_code(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_code = false;
    let mut in_quote = false;
    for c in line.chars() {
        match c {
            '`' if !in_quote => in_code = !in_code,
            '"' if !in_code => in_quote = !in_quote,
            '“' if !in_code => in_quote = true,
            '”' if !in_code => in_quote = false,
            _ if !in_code && !in_quote => out.push(c),
            _ => {}
        }
    }
    out
}

/// Sentences of a paragraph: each line (list items stand alone), split after
/// `.`, `!`, `?` and their full-width forms. Markdown emphasis and list
/// markers are dropped.
fn sentences(paragraph: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in paragraph.lines() {
        let line = line
            .trim_start_matches(|c: char| matches!(c, '-' | '*' | '+' | '#') || c.is_whitespace());
        let line = line.replace("**", "");
        let mut current = String::new();
        for c in line.chars() {
            current.push(c);
            if matches!(c, '.' | '!' | '?' | '。' | '！' | '？') {
                push_sentence(&mut out, &mut current);
            }
        }
        push_sentence(&mut out, &mut current);
    }
    out
}

fn push_sentence(out: &mut Vec<String>, current: &mut String) {
    let sentence = current.trim();
    // A lone "?" or a numbered marker like "1." is not a sentence.
    if sentence.chars().filter(|c| c.is_alphanumeric()).count() >= 2 {
        out.push(sentence.to_owned());
    }
    current.clear();
}

fn truncate(sentence: &str) -> String {
    if sentence.chars().count() <= MAX_QUESTION_CHARS {
        return sentence.to_owned();
    }
    let mut out: String = sentence.chars().take(MAX_QUESTION_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn a_decision_in_the_middle_counts_even_with_notes_after_it() {
        // The jiun-mbp comp_use case that motivated this module.
        let response = "제 제안 (v2 설계 방향)\n- 스스로 승인 막기: 설정이 켜져 있으면 입력하지 않습니다.\n\n\
            결정해 주실 것: 설정 끄기와 '항상 허용'에 Touch ID나 암호를 요구해도 될까요? \
            괜찮으시면 이 방향으로 v2 설계를 쓰고 다시 리뷰받겠습니다.\n\n그 밖에\n\
            - PR #12(macOS CI 수정)는 아직 열려 있습니다.\n- 설계 리뷰 전문은 위 Codex 회신에 있습니다.";
        let question = classify(response).expect("asks for a decision");
        assert!(question.contains("Touch ID"), "{question}");
    }

    #[test]
    fn closing_questions_count() {
        for response in [
            "작업을 마쳤습니다.\n\n이 방향으로 진행할까요?",
            "Done with the parser.\n\nShould I also migrate the old configs?",
            "두 가지 안이 있습니다.\n\nA안과 B안 중 어느 쪽으로 갈지 알려 주시면 바로 진행하겠습니다.",
            "Tests pass.\n\nDo you want me to open the PR now",
            "끝났습니다. 머지해도 될까요？",
        ] {
            assert!(classify(response).is_some(), "{response}");
        }
    }

    #[test]
    fn finished_turns_and_courtesy_closers_do_not_count() {
        for response in [
            "PR #416을 머지했고 배포까지 끝났습니다.",
            "All tests pass and the release is published.",
            "완료했습니다.\n\n더 필요한 것 있으면 말씀해 주세요.",
            "Done. Let me know if you have any questions!",
            "Fixed. Anything else?",
            "",
        ] {
            assert_eq!(classify(response), None, "{response}");
        }
    }

    #[test]
    fn a_question_the_agent_answered_early_on_does_not_count() {
        let response = "왜 실패했을까요? 원인은 캐시였습니다.\n\n캐시 키를 고쳤습니다.\n\n\
            테스트가 통과합니다.\n\nCI도 초록입니다.";
        assert_eq!(classify(response), None);
    }

    #[test]
    fn a_self_answered_question_or_an_offer_asks_nothing() {
        for response in [
            "Why did CI fail? The API key was missing. I fixed it and tests pass.",
            "왜 실패했을까요? 키가 없었습니다. 고쳤고 테스트가 통과합니다.",
            "All done and merged.\n\nWould you like a walkthrough of the changes?",
            "배포했습니다. 원하시면 변경 내용을 정리해 드릴까요?",
            r#"The error said "is the socket open?" and the fix closes it first."#,
        ] {
            assert_eq!(classify(response), None, "{response}");
        }
    }

    #[test]
    fn a_question_followed_by_what_happens_next_still_asks() {
        for (response, expected) in [
            (
                "Design is ready.\n\nCan I drop the legacy path? If so, I'll implement it today.",
                "Can I drop the legacy path?",
            ),
            (
                "두 안을 비교했습니다.\n\nB안으로 갈까요? 괜찮으시면 바로 구현하겠습니다.",
                "B안으로 갈까요?",
            ),
            (
                "A+B로 진행할까요? 배포는 백그라운드에서 계속 진행 중입니다.",
                "A+B로 진행할까요?",
            ),
        ] {
            assert_eq!(classify(response).as_deref(), Some(expected), "{response}");
        }
    }

    #[test]
    fn a_long_response_still_asks_after_truncation() {
        let body = "본문입니다. ".repeat(700);
        for closing in ["머지해도 될까요?", "Should I merge it now?"] {
            let full = format!("{body}\n\n{closing}");
            let bounded = crate::adapters::hook::truncate_response(full);
            assert_eq!(classify(&bounded).as_deref(), Some(closing));
        }
    }

    #[test]
    fn question_marks_in_code_and_quotes_ask_nothing() {
        let response = "정규식을 `a?b` 로 바꿨습니다.\n\n```rust\nlet x = y?;\n```\n\n> 왜 안 될까요?\n\n끝났습니다.";
        assert_eq!(classify(response), None);
    }

    #[test]
    fn the_question_is_trimmed_for_display() {
        let long = format!("{}할까요?", "가".repeat(400));
        let question = classify(&long).unwrap();
        assert_eq!(question.chars().count(), 200);
        assert!(question.ends_with('…'));
    }
}
