//! Public-API tests for the transcript entry tree.
//!
//! Branching is what lets a conversation return to an earlier point and
//! continue differently — "undo that exchange and try another instruction",
//! editing a message and re-running, or exploring two approaches from a shared
//! prefix — without starting a new session and replaying a prefix by hand.

use mentra::{
    AgentTranscript, ContentBlock, Message,
    transcript::{EntryId, TranscriptItem},
};

fn user(text: &str) -> TranscriptItem {
    TranscriptItem::user_turn(Message::user(ContentBlock::text(text)))
}

fn assistant(text: &str) -> TranscriptItem {
    TranscriptItem::assistant_turn(Message::assistant(ContentBlock::text(text)))
}

fn transcript_of(texts: &[&str]) -> AgentTranscript {
    let mut transcript = AgentTranscript::default();
    for (index, text) in texts.iter().enumerate() {
        if index % 2 == 0 {
            transcript.push(user(text));
        } else {
            transcript.push(assistant(text));
        }
    }
    transcript
}

fn texts(transcript: &AgentTranscript) -> Vec<String> {
    transcript
        .items()
        .iter()
        .map(TranscriptItem::text)
        .collect()
}

#[test]
fn appending_hangs_each_entry_from_the_leaf() {
    let transcript = transcript_of(&["one", "two", "three"]);
    let items = transcript.items();

    assert_eq!(items[0].parent_id, None, "the first entry is a root");
    assert_eq!(items[1].parent_id.as_ref(), Some(&items[0].id));
    assert_eq!(items[2].parent_id.as_ref(), Some(&items[1].id));
    assert_eq!(transcript.leaf(), Some(&items[2].id));
}

#[test]
fn branching_rewinds_the_active_path_without_deleting_anything() {
    let mut transcript = transcript_of(&["ask", "answer", "follow up"]);
    let rewind_to = transcript.items()[0].id.clone();

    let moved = transcript
        .branch_from(&rewind_to)
        .expect("entry is on path");

    assert_eq!(moved, 2, "two entries leave the active path");
    assert_eq!(texts(&transcript), vec!["ask"]);
    assert_eq!(transcript.leaf(), Some(&rewind_to));
    assert_eq!(
        transcript.archived().len(),
        2,
        "abandoned entries stay in the transcript"
    );
}

#[test]
fn a_new_turn_after_branching_becomes_a_sibling() {
    let mut transcript = transcript_of(&["ask", "first answer"]);
    let fork_point = transcript.items()[0].id.clone();

    transcript.branch_from(&fork_point).expect("on path");
    transcript.push(assistant("second answer"));

    let children = transcript.children(&fork_point);
    assert_eq!(children.len(), 2, "the fork point now has two paths");

    let child_texts: Vec<String> = children.iter().map(|item| item.text()).collect();
    assert!(child_texts.contains(&"first answer".to_string()));
    assert!(child_texts.contains(&"second answer".to_string()));

    // Only the new path is live.
    assert_eq!(texts(&transcript), vec!["ask", "second answer"]);
}

#[test]
fn an_abandoned_branch_can_be_returned_to() {
    let mut transcript = transcript_of(&["ask", "first answer"]);
    let fork_point = transcript.items()[0].id.clone();
    let first_answer = transcript.items()[1].id.clone();

    transcript.branch_from(&fork_point).expect("on path");
    transcript.push(assistant("second answer"));

    // The abandoned entry is still addressable, which is what makes this a
    // branch rather than a truncation.
    let recovered = transcript.entry(&first_answer).expect("still present");
    assert_eq!(recovered.text(), "first answer");
}

#[test]
fn branching_to_an_unknown_entry_is_refused() {
    let mut transcript = transcript_of(&["ask"]);

    let error = transcript
        .branch_from(&EntryId::new())
        .expect_err("an entry that was never appended is not a branch point");

    assert!(error.to_string().contains("no entry"));
}

#[test]
fn branching_to_the_leaf_changes_nothing() {
    let mut transcript = transcript_of(&["ask", "answer"]);
    let leaf = transcript.leaf().cloned().expect("a leaf");

    let moved = transcript.branch_from(&leaf).expect("the leaf is on path");

    assert_eq!(moved, 0);
    assert_eq!(texts(&transcript), vec!["ask", "answer"]);
    assert!(transcript.archived().is_empty());
}

#[test]
fn the_tree_survives_a_round_trip() {
    let mut transcript = transcript_of(&["ask", "first answer"]);
    let fork_point = transcript.items()[0].id.clone();
    transcript.branch_from(&fork_point).expect("on path");
    transcript.push(assistant("second answer"));

    let encoded = serde_json::to_string(&transcript).expect("serializes");
    let decoded: AgentTranscript = serde_json::from_str(&encoded).expect("deserializes");

    assert_eq!(decoded, transcript);
    assert_eq!(
        decoded.children(&fork_point).len(),
        2,
        "both paths survive persistence"
    );
}

#[test]
fn a_transcript_written_before_entries_had_ids_still_loads_linked() {
    // Build the shape mentra persisted previously by stripping the tree
    // fields back out of a current transcript, so the fixture cannot drift
    // from the real serialization format.
    let modern = transcript_of(&["ask", "answer"]);
    let mut encoded: serde_json::Value = serde_json::to_value(&modern).expect("serializes");
    for item in encoded["items"]
        .as_array_mut()
        .expect("items is an array")
        .iter_mut()
    {
        let object = item.as_object_mut().expect("each item is an object");
        object.remove("id");
        object.remove("parent_id");
    }

    let transcript: AgentTranscript =
        serde_json::from_value(encoded).expect("a pre-tree transcript still deserializes");
    let items = transcript.items();

    assert_eq!(items.len(), 2);
    assert_eq!(texts(&transcript), vec!["ask", "answer"]);
    assert_eq!(items[0].parent_id, None, "the first entry is a root");
    assert_eq!(
        items[1].parent_id.as_ref(),
        Some(&items[0].id),
        "migration links the chain, so a legacy transcript can be branched"
    );
}
