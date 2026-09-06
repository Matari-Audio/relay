//! Best-effort clipboard write: arboard first, then the usual CLI tools.

use std::io::Write;
use std::process::{Command, Stdio};

/// Returns `true` when some backend accepted the text.
pub fn copy(text: &str) -> bool {
    if arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text.to_owned()))
        .is_ok()
    {
        return true;
    }
    [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
    ]
    .iter()
    .any(|(program, args)| pipe_copy(program, args, text))
}

fn pipe_copy(program: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    written && child.wait().is_ok_and(|status| status.success())
}
