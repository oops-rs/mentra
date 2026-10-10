use a2a_lf::{Artifact, Role, TaskStatus};
use serde_json::json;

use super::*;

fn reply(parts: Vec<Part>) -> String {
    remote_reply("nous", &Message::new(Role::Agent, parts), 4096)
}

#[test]
fn a_data_part_is_shown_as_indented_json() {
    let text = reply(vec![Part::data(json!({"rows": [1, 2]}))]);
    assert!(
        text.contains("[data]\n{\n  \"rows\": [\n    1,\n    2\n  ]\n}"),
        "{text}"
    );
}

#[test]
fn an_inline_text_file_is_shown_by_name_type_and_size_with_its_contents() {
    for media_type in [
        "text/markdown",
        "application/json",
        "application/ld+json",
        "application/xml; charset=utf-8",
        "APPLICATION/YAML",
    ] {
        let part = Part::raw(b"key: value".to_vec())
            .with_filename("notes.txt")
            .with_media_type(media_type);
        let text = reply(vec![part]);
        assert!(
            text.contains(&format!(
                "[file notes.txt ({media_type}): 10 bytes]\nkey: value"
            )),
            "{media_type}: {text}"
        );
    }
}

#[test]
fn an_inline_binary_file_is_described_and_not_shown() {
    for (bytes, media_type) in [
        (b"%PDF-1.7".to_vec(), Some("application/pdf")),
        (b"no type given".to_vec(), None),
        (vec![0xff, 0xfe, 0x00], Some("text/plain")),
    ] {
        let mut part = Part::raw(bytes.clone()).with_filename("blob");
        part.media_type = media_type.map(str::to_string);
        let text = reply(vec![part]);
        assert!(
            text.contains(&format!("{} bytes inline, not shown]", bytes.len())),
            "{media_type:?}: {text}"
        );
    }
}

#[test]
fn every_part_stays_inside_the_frame_and_its_bound() {
    let task = Task {
        id: "t1".into(),
        context_id: "c1".into(),
        status: TaskStatus {
            state: TaskState::Completed,
            message: None,
            timestamp: None,
        },
        artifacts: Some(vec![Artifact {
            artifact_id: "a1".into(),
            name: None,
            description: None,
            parts: vec![
                Part::data(json!({"big": "x".repeat(400)})),
                Part::raw("y".repeat(400).into_bytes()).with_media_type("text/plain"),
            ],
            metadata: None,
            extensions: None,
        }]),
        history: None,
        metadata: None,
    };
    let text = remote_output("nous", &task, 256);
    assert!(text.starts_with("--- begin output from remote agent `nous` ---\n"));
    assert!(text.ends_with("\n--- end output from remote agent `nous` ---"));
    assert!(text.contains("[truncated: "), "{text}");
}

#[test]
fn a_remote_line_is_one_bounded_line_without_control_characters() {
    assert_eq!(
        remote_line("reading\n\tthe \u{1b}[31mcode\r\n", 64),
        "reading the [31mcode"
    );
    assert_eq!(remote_line("éé", 3), "é…");
    assert_eq!(remote_line(" \n ", 64), "");
}
