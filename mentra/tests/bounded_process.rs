//! The spawn primitive as an embedder reaches it: through `mentra::process`,
//! with no runtime, no executor trait, and no crate-internal helper.
//!
//! Gated to unix because the fixtures are `/bin/sh` scripts. The confinement
//! under test is portable; a Windows equivalent of "background a process and
//! hold the pipe" would test the fixture rather than the primitive.
#![cfg(unix)]

use std::time::Duration;

use mentra::process::{BoundedCommand, Completion};

const CAP: usize = 64 * 1024;

#[tokio::test]
async fn a_host_runs_its_own_program_with_a_payload_and_reads_the_answer() {
    // The exchange both of basis's subprocess seams are built around: a JSON
    // payload in, whatever the program prints out, one deadline over it all.
    let completion = BoundedCommand::new("/bin/sh", Duration::from_secs(5), CAP)
        .arg("-c")
        .arg("cat; echo ' done' >&2; exit 7")
        .env("PATH", std::env::var("PATH").expect("path available"))
        .stdin(r#"{"decision":"allow"}"#)
        .run()
        .await
        .expect("the program is supervised");

    let Completion::Exited {
        code,
        stdout,
        stderr,
    } = completion
    else {
        panic!("the program exited inside its budget");
    };
    assert_eq!(code, Some(7));
    assert_eq!(stdout.to_string_lossy(), r#"{"decision":"allow"}"#);
    assert_eq!(stderr.to_string_lossy(), " done\n");
    assert!(!stdout.truncated());
}

#[tokio::test]
async fn a_host_gets_the_process_tree_killed_at_its_deadline() {
    let started = std::time::Instant::now();

    let completion = BoundedCommand::shell("sleep 60", Duration::from_millis(200), CAP)
        .env("PATH", std::env::var("PATH").expect("path available"))
        .run()
        .await
        .expect("the program is supervised");

    assert!(completion.timed_out(), "{completion:?}");
    assert_eq!(completion.code(), None);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the deadline decides how long this takes: {:?}",
        started.elapsed()
    );
}
