//! One line of share play's control stream: refused when over 4 KiB, with a second line in it,
//! or not UTF-8; otherwise a message read from it is written back so that it reads the same.

#![no_main]

use anyplay_share::control::{ControlError, MAX_LINE, encode_line, parse_line};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match parse_line(data) {
        Ok(Some(message)) => {
            let line = encode_line(&message).expect("a message that was read fits a line");
            assert!(line.len() <= MAX_LINE && line.ends_with(b"\n"));
            assert_eq!(parse_line(&line), Ok(Some(message)));
        }
        Ok(None) => {}
        Err(ControlError::TooLong) => assert!(data.len() > MAX_LINE),
        Err(ControlError::MoreThanOneLine) => {
            let body = data.strip_suffix(b"\n").unwrap_or(data);
            assert!(body.contains(&b'\n'));
        }
        Err(ControlError::NotUtf8) => {
            let body = data.strip_suffix(b"\n").unwrap_or(data);
            assert!(std::str::from_utf8(body).is_err());
        }
        Err(ControlError::Invalid(_)) => {}
    }
});
