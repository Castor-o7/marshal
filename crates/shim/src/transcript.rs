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
        // Cheap test first: most lines name none of the ids.
        let named: Vec<&String> = ids
            .iter()
            .filter(|id| !found.contains(id) && line.contains(id.as_str()))
            .collect();
        if named.is_empty() || !went_to_model(line) {
            continue;
        }
        found.extend(named.into_iter().cloned());
        if found.len() == ids.len() {
            break;
        }
    }
    found
}

/// Whether a transcript line is a record of content sent to the model.
fn went_to_model(line: &str) -> bool {
    let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    record
        .get("type")
        .and_then(|t| t.as_str())
        .is_some_and(|t| MODEL_BOUND.contains(&t))
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
