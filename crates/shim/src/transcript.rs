//! Finding offered mail in a Claude session's transcript (MAR-82, designed in
//! docs/two-phase-delivery.md).
//!
//! A Claude session's waiter hands mail to Claude Code, which holds it until
//! the running turn ends, so the daemon only records it *offered*. The
//! session's later hooks read the tail of its transcript (`transcript_path`,
//! in every hook's input) for the inbox block's message ids, and an id found
//! in a record that went to the model is confirmed *seen*.
//!
//! Claude Code writes a held rewake to the transcript twice: first as a
//! `queue-operation` record (`"operation": "enqueue"`) while it's held, which
//! is exactly the state this exists to tell apart, and again as a `user` or
//! `attachment` record when it goes to the model. Only the second kind counts.
//! (It also writes an `api-request-blob` then, which isn't counted: each
//! delivery recorded so far also has a `user` or `attachment` record, and
//! counting less can only leave mail offered, never lose it.)
//!
//! The transcript is Claude Code's own, undocumented format. Everything here
//! fails safe: a file that can't be read, a line that isn't JSON, or a record
//! of any other type matches nothing, and the mail stays offered, to be
//! offered again after the daemon's offer timeout. It can only confirm too
//! little, never too much.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the transcript's end is read. The rewake goes to the model at
/// the start of the next turn, and the hooks that confirm it run during or
/// just after that turn, so the record is near the end.
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// Record types that mean the content went to the model.
const MODEL_BOUND: &[&str] = &["user", "attachment"];

/// Which of `ids` appear in a record of `transcript` that went to the model.
pub fn seen(transcript: &Path, ids: &[String]) -> Vec<String> {
    if ids.is_empty() {
        return Vec::new();
    }
    match read_tail(transcript) {
        Some(text) => seen_in(&text, ids),
        None => Vec::new(),
    }
}

/// `seen` over transcript text already read.
pub fn seen_in(text: &str, ids: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in text.lines() {
        // Cheap test first: most lines name none of the ids, or no inbox.
        if !line.contains("marshal_inbox") || !ids.iter().any(|id| line.contains(id.as_str())) {
            continue;
        }
        for id in delivered_ids(line) {
            if ids.contains(&id) && !found.contains(&id) {
                found.push(id);
            }
        }
        if found.len() == ids.len() {
            break;
        }
    }
    found
}

/// The ids named by inbox blocks that one transcript line shows went to the
/// model: the `ids` attribute of each `<marshal_inbox …>` opening tag in the
/// record's text, outside any tool result. Only the daemon writes that tag:
/// it escapes `<` in every body and handle, so a message can't forge one.
/// A tool result can hold one, though (a session that reads a log or a
/// transcript), and that isn't delivery, so tool results are skipped.
fn delivered_ids(line: &str) -> Vec<String> {
    let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let model_bound = record
        .get("type")
        .and_then(|t| t.as_str())
        .is_some_and(|t| MODEL_BOUND.contains(&t));
    if !model_bound {
        return Vec::new();
    }
    let mut ids = Vec::new();
    walk_text(&record, &mut |text| ids.extend(tag_ids(text)));
    ids
}

/// Every string in `value`, leaving out tool calls and their results.
fn walk_text(value: &serde_json::Value, visit: &mut impl FnMut(&str)) {
    match value {
        serde_json::Value::String(s) => visit(s),
        serde_json::Value::Array(items) => items.iter().for_each(|v| walk_text(v, visit)),
        serde_json::Value::Object(map) => {
            let kind = map.get("type").and_then(|t| t.as_str());
            if matches!(kind, Some("tool_result" | "tool_use")) || map.contains_key("toolUseResult")
            {
                return;
            }
            map.values().for_each(|v| walk_text(v, visit));
        }
        _ => {}
    }
}

/// The ids in each `<marshal_inbox …>` opening tag's `ids="a,b"` attribute,
/// each exactly as written, so one id can't match another it's a prefix of.
fn tag_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<marshal_inbox") {
        let after = &rest[start + "<marshal_inbox".len()..];
        let Some(end) = after.find('>') else { break };
        let tag = &after[..end];
        if let Some(attr) = tag.find(" ids=\"") {
            let value = &tag[attr + " ids=\"".len()..];
            if let Some(close) = value.find('"') {
                ids.extend(
                    value[..close]
                        .split(',')
                        .filter(|id| !id.is_empty())
                        .map(str::to_string),
                );
            }
        }
        rest = &after[end..];
    }
    ids
}

/// Which of `ids` a Codex rollout file (`transcript_path` in Codex's hook
/// input, MAR-84) shows the model received. Codex records a hook's
/// `additionalContext` as a developer `response_item` *before* it samples,
/// and records it even when the turn then stops, so that line alone proves
/// nothing (docs/codex-two-phase-delivery.md, from Codex's source at
/// `rust-v0.163.0`). An id counts only when model output (an `assistant`
/// message, `reasoning` or a `function_call`) comes after its developer line,
/// in file order. Output before it, from an earlier turn, doesn't count. Only
/// this file is read: a rollout resumed from another isn't followed.
pub fn seen_codex(rollout: &Path, ids: &[String]) -> Vec<String> {
    if ids.is_empty() {
        return Vec::new();
    }
    match read_tail(rollout) {
        Some(text) => codex_seen_in(&text, ids),
        None => Vec::new(),
    }
}

/// `seen_codex` over rollout text already read.
pub fn codex_seen_in(text: &str, ids: &[String]) -> Vec<String> {
    // Ids whose developer line has been passed, still waiting for model output.
    let mut waiting: Vec<String> = Vec::new();
    let mut found: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if record.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = record.get("payload") else {
            continue;
        };
        let kind = payload.get("type").and_then(|t| t.as_str());
        let role = payload.get("role").and_then(|r| r.as_str());
        match (kind, role) {
            (Some("message"), Some("developer")) => {
                if !line.contains("marshal_inbox") {
                    continue;
                }
                let mut named = Vec::new();
                if let Some(content) = payload.get("content") {
                    walk_text(content, &mut |text| named.extend(tag_ids(text)));
                }
                for id in named {
                    if ids.contains(&id) && !found.contains(&id) && !waiting.contains(&id) {
                        waiting.push(id);
                    }
                }
            }
            (Some("message"), Some("assistant"))
            | (Some("reasoning"), _)
            | (Some("function_call"), _) => {
                found.append(&mut waiting);
                if found.len() == ids.len() {
                    break;
                }
            }
            _ => {}
        }
    }
    found
}

/// The last `TAIL_BYTES` of the file, from the first whole line on.
fn read_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let text = if start > 0 {
        // Started mid-line: drop the partial first line.
        text.split_once('\n')
            .map_or("", |(_, rest)| rest)
            .to_string()
    } else {
        text.into_owned()
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records shaped as Claude Code 2.1.319 writes them for a held rewake:
    /// the top-level `type` (and `operation`) values were counted read-only
    /// from a live session's transcript on Murph, 2026-11-09 (MAR-79). The
    /// contents here are synthetic.
    fn records(id: &str) -> [String; 6] {
        let inbox = format!(
            "<marshal_inbox count=\"1\" ids=\"{id}\">\n- from wise-leopard [response_required=false]: hi\n</marshal_inbox>"
        );
        let content = serde_json::to_string(&inbox).unwrap();
        [
            format!(r#"{{"type":"queue-operation","operation":"enqueue","content":{content}}}"#),
            format!(r#"{{"type":"queue-operation","operation":"remove","content":{content}}}"#),
            format!(r#"{{"type":"user","message":{{"role":"user","content":{content}}}}}"#),
            format!(
                r#"{{"type":"attachment","attachment":{{"type":"hook_success","content":{content}}}}}"#
            ),
            format!(
                r#"{{"type":"attachment","attachment":{{"type":"queued_command","prompt":{content}}}}}"#
            ),
            format!(
                r#"{{"type":"attachment","attachment":{{"type":"hook_additional_context","content":[{content}]}}}}"#
            ),
        ]
    }

    const A: &str = "01a1c1f1-7ab6-7b93-b529-71d184728a5a";
    const B: &str = "01a1c1ad-65a8-7f92-a209-d98bd4b237f9";

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_held_rewake_is_not_seen() {
        // Only the enqueue (and its remove) is written: still held.
        let r = records(A);
        let text = format!("{}\n{}\n", r[0], r[1]);
        assert!(seen_in(&text, &ids(&[A])).is_empty());
    }

    #[test]
    fn mail_in_a_record_sent_to_the_model_is_seen() {
        let r = records(A);
        for (i, record) in r.iter().enumerate().skip(2) {
            let text = format!("{}\n{record}\n", r[0]);
            assert_eq!(
                seen_in(&text, &ids(&[A])),
                ids(&[A]),
                "record {i}: {record}"
            );
        }
    }

    #[test]
    fn only_the_ids_found_are_seen() {
        let text = format!("{}\n{}\n", records(A)[2], records(B)[0]);
        assert_eq!(seen_in(&text, &ids(&[A, B])), ids(&[A]));
    }

    #[test]
    fn a_format_it_doesnt_know_matches_nothing() {
        // A Claude Code that renamed its record types, or wrote something
        // other than JSON lines: fail safe, the mail stays offered.
        let content = serde_json::to_string(&format!("ids=\"{A}\"")).unwrap();
        for text in [
            format!(r#"{{"kind":"user","content":{content}}}"#),
            format!(r#"{{"type":"message","content":{content}}}"#),
            format!("user: ids=\"{A}\""),
            format!(r#"{{"type":"user","content":{content}"#), // cut off
            String::new(),
        ] {
            assert!(seen_in(&text, &ids(&[A])).is_empty(), "{text}");
        }
    }

    /// Records recorded from Claude Code 2.1.321 for two held rewakes, scrubbed
    /// to their shapes (`tests/fixtures/claude-code-2.1.321/README.md`).
    const RECORDED: &str =
        include_str!("../tests/fixtures/claude-code-2.1.321/transcript-rewake.jsonl");

    fn recorded(id: &str) -> Vec<String> {
        RECORDED.lines().map(|l| l.replace("{ID}", id)).collect()
    }

    fn kind(line: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let t = v["type"].as_str().unwrap_or_default();
        match t {
            "queue-operation" => format!("{t}/{}", v["operation"].as_str().unwrap_or_default()),
            "attachment" => format!(
                "{t}/{}",
                v["attachment"]["type"].as_str().unwrap_or_default()
            ),
            _ => t.to_string(),
        }
    }

    #[test]
    fn the_recorded_records_are_the_shapes_the_matcher_expects() {
        let kinds: Vec<String> = recorded(A).iter().map(|l| kind(l)).collect();
        assert_eq!(
            kinds,
            [
                "queue-operation/enqueue",
                "queue-operation/remove",
                "api-request-blob",
                "attachment/queued_command",
                "queue-operation/enqueue",
                "api-request-blob",
                "user",
            ],
            "Claude Code's records changed shape: re-record the fixture and check the matcher"
        );
    }

    #[test]
    fn a_recorded_held_rewake_is_not_seen() {
        // Each delivery's first record is the enqueue, written while it's held.
        let r = recorded(A);
        assert!(seen_in(&r[0], &ids(&[A])).is_empty());
        assert!(seen_in(&format!("{}\n{}", r[0], r[1]), &ids(&[A])).is_empty());
        assert!(seen_in(&r[4], &ids(&[A])).is_empty());
    }

    #[test]
    fn a_recorded_delivery_to_the_model_is_seen() {
        let r = recorded(A);
        // Busy: enqueue, remove, request, then the queued_command attachment.
        assert_eq!(seen_in(&r[..4].join("\n"), &ids(&[A])), ids(&[A]));
        // Idle: enqueue, request, then the user record.
        assert_eq!(seen_in(&r[4..].join("\n"), &ids(&[A])), ids(&[A]));
    }

    /// A `user` record Claude Code 2.1.321 wrote for a Bash tool result whose
    /// output was an inbox block (a file `cat`), recorded in MAR-82's check 5
    /// session and scrubbed: the block is in `message.content[].tool_result`
    /// and again in `toolUseResult.stdout`.
    const RECORDED_TOOL_RESULT: &str =
        include_str!("../tests/fixtures/claude-code-2.1.321/transcript-tool-result.jsonl");

    #[test]
    fn a_recorded_tool_result_holding_an_inbox_block_is_not_delivery() {
        let line = RECORDED_TOOL_RESULT.trim().replace("{ID}", A);
        assert!(
            line.contains(&format!(r#"ids=\"{A}\""#)),
            "fixture names the id"
        );
        assert!(seen_in(&line, &ids(&[A])).is_empty(), "{line}");
    }

    #[test]
    fn an_inbox_block_in_a_tool_result_is_not_delivery() {
        // A session that `cat`s a log or a transcript gets inbox blocks back
        // as tool output, in a `user` record. That isn't the model being
        // handed the mail.
        let inbox = serde_json::to_string(&format!(
            "<marshal_inbox count=\"1\" ids=\"{A}\">\n- from x: hi\n</marshal_inbox>"
        ))
        .unwrap();
        for line in [
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":{inbox}}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","content":[{{"type":"text","text":{inbox}}}]}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":"ok"}},"toolUseResult":{{"stdout":{inbox}}}}}"#
            ),
        ] {
            assert!(seen_in(&line, &ids(&[A])).is_empty(), "{line}");
        }
    }

    #[test]
    fn an_id_outside_an_inbox_tag_is_not_delivery() {
        // In a body, a reply, or anywhere but the daemon's opening tag.
        let text = serde_json::to_string(&format!(
            "marshal_inbox mentioned; the message {A} and ids=\"{A}\" in prose"
        ))
        .unwrap();
        let line = format!(r#"{{"type":"user","message":{{"role":"user","content":{text}}}}}"#);
        assert!(seen_in(&line, &ids(&[A])).is_empty(), "{line}");
    }

    #[test]
    fn an_id_matches_only_itself_not_one_it_begins() {
        let longer = format!("{A}0");
        let inbox = serde_json::to_string(&format!(
            "<marshal_inbox count=\"1\" ids=\"{longer},{B}\">\n</marshal_inbox>"
        ))
        .unwrap();
        let line = format!(r#"{{"type":"user","message":{{"role":"user","content":{inbox}}}}}"#);
        assert_eq!(seen_in(&line, &ids(&[A, B])), ids(&[B]));
    }

    // ---- Codex rollout records (MAR-84). Constructed from Codex's source at
    // `rust-v0.163.0` (`history/src/lib.rs` RolloutLine, `rollout_payload.rs`,
    // `protocol/src/models.rs` ResponseItem); not yet recorded from a live
    // Codex (pending one on Murph).

    fn codex_developer(id: &str) -> String {
        let text = serde_json::to_string(&format!(
            "<marshal_inbox count=\"1\" ids=\"{id}\">\n- from x [response_required=false]: hi\n</marshal_inbox>"
        ))
        .unwrap();
        format!(
            r#"{{"timestamp":"t","type":"response_item","payload":{{"type":"message","role":"developer","content":[{{"type":"input_text","text":{text}}}]}}}}"#
        )
    }

    const CODEX_ASSISTANT: &str = r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#;
    const CODEX_REASONING: &str =
        r#"{"timestamp":"t","type":"response_item","payload":{"type":"reasoning","summary":[]}}"#;
    const CODEX_CALL: &str = r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c"}}"#;
    const CODEX_USER: &str = r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}}"#;
    const CODEX_META: &str = r#"{"timestamp":"t","type":"session_meta","payload":{"id":"s"}}"#;

    #[test]
    fn codex_context_followed_by_model_output_is_seen() {
        for output in [CODEX_ASSISTANT, CODEX_REASONING, CODEX_CALL] {
            let text = [CODEX_META, CODEX_USER, &codex_developer(A), output].join("\n");
            assert_eq!(codex_seen_in(&text, &ids(&[A])), ids(&[A]), "{output}");
        }
    }

    #[test]
    fn codex_context_with_no_output_after_it_is_not_seen() {
        // Recorded before sampling, and recorded when the turn then stops.
        let text = [CODEX_USER, &codex_developer(A)].join("\n");
        assert!(codex_seen_in(&text, &ids(&[A])).is_empty());
    }

    #[test]
    fn codex_output_before_the_context_proves_nothing() {
        // An earlier turn's output, in file order before the developer line.
        let text = [
            CODEX_ASSISTANT,
            CODEX_REASONING,
            CODEX_USER,
            &codex_developer(A),
        ]
        .join("\n");
        assert!(codex_seen_in(&text, &ids(&[A])).is_empty());
    }

    #[test]
    fn codex_tool_output_holding_an_inbox_block_is_not_context() {
        // A function_call_output whose output is an inbox block, then output.
        let block =
            serde_json::to_string(&format!("<marshal_inbox count=\"1\" ids=\"{A}\">")).unwrap();
        let tool = format!(
            r#"{{"timestamp":"t","type":"response_item","payload":{{"type":"function_call_output","call_id":"c","output":{block}}}}}"#
        );
        let text = [tool.as_str(), CODEX_ASSISTANT].join("\n");
        assert!(codex_seen_in(&text, &ids(&[A])).is_empty());
    }

    #[test]
    fn codex_a_later_turn_confirms_what_an_earlier_stopped_turn_recorded() {
        // The first turn stopped before sampling; the next one's output comes
        // with that context still in history.
        let text = [
            &codex_developer(A),
            CODEX_USER,
            &codex_developer(B),
            CODEX_ASSISTANT,
        ]
        .join("\n");
        assert_eq!(codex_seen_in(&text, &ids(&[A, B])), ids(&[A, B]));
    }

    #[test]
    fn an_unreadable_transcript_matches_nothing() {
        assert!(seen(Path::new("/nonexistent/transcript.jsonl"), &ids(&[A])).is_empty());
    }

    #[test]
    fn the_tail_is_read_from_a_whole_line() {
        let dir = std::env::temp_dir().join(format!("marshal-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        // A long first part pushes the record near the end of a big file.
        let filler = format!("{}\n", r#"{"type":"assistant","x":"y"}"#).repeat(200_000);
        std::fs::write(&path, format!("{filler}{}\n", records(A)[2])).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > TAIL_BYTES);
        assert_eq!(seen(&path, &ids(&[A])), ids(&[A]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
