//! Process loopback capture asks the system for the sound of one process tree. Whatever the
//! system answers (a client, or a refusal on a machine without sound), asking must leave the
//! worker running: the request is built on the stack and is not the system's to free.

#![cfg(windows)]

use anyplay_share_worker::loopback::LoopbackSource;

#[test]
fn asking_for_a_process_s_sound_leaves_the_worker_running() {
    for _ in 0..3 {
        match LoopbackSource::open(std::process::id()) {
            Ok(source) => drop(source),
            Err(why) => eprintln!("no sound on this machine: {why}"),
        }
    }
}
