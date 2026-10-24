//! One line between the launcher and the net service, read the way each side reads it: text that
//! is not UTF-8 is refused before parsing. A command or an output read from a line is written back
//! so that it reads the same.

#![no_main]

use anyplay_control::{command_line, output_line, parse_command, parse_output};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let line = text.trim_end();
    if let Ok((id, command)) = parse_command(line) {
        assert_eq!(
            parse_command(&command_line(id, &command)),
            Ok((id, command))
        );
    }
    if let Some(output) = parse_output(line) {
        assert_eq!(parse_output(&output_line(&output)), Some(output));
    }
});
