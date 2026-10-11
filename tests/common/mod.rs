//! Helpers shared by the integration tests.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

pub const BIN: &str = env!("CARGO_BIN_EXE_pkg-to-anyps5");

pub fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).env_remove("NO_COLOR").output().expect("run the binary")
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("UTF-8 output")
}

/// A fresh directory for one test.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pkg-to-anyps5-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn has_script() -> bool {
    let found = Command::new("script").arg("--version").output().is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("note: util-linux `script` not found; skipping the terminal test");
    }
    found
}

/// Runs a shell command line in a pseudo-terminal and returns everything
/// written to it.
pub fn in_terminal(command: &str, env: &[(&str, &str)]) -> String {
    let mut script = Command::new("script");
    script.args(["-qfec", command, "/dev/null"]).env("SHELL", "/bin/sh").env_remove("NO_COLOR").env("TERM", "xterm-256color");
    for (key, value) in env {
        script.env(key, value);
    }
    let output = script.output().expect("run script");
    text(&output.stdout)
}

/// The text a terminal shows, without escape sequences or CAN.
pub fn visible(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
            }
            '\x18' => {}
            c => out.push(c),
        }
    }
    out
}

/// Terminal columns of a character, for the scripts the demo prints:
/// ASCII, box drawing, braille, kana, CJK and emoji.
pub fn columns(s: &str) -> usize {
    s.chars()
        .map(|c| match c as u32 {
            0..=0x1f | 0x7f..=0x9f | 0x300..=0x36f | 0x200b..=0x200f | 0xfe00..=0xfe0f => 0,
            0x1100..=0x115f | 0x2e80..=0x303e | 0x3041..=0x33ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7a3 => 2,
            0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x1f300..=0x1f64f | 0x1f680..=0x1f6ff | 0x1f900..=0x1f9ff => 2,
            _ => 1,
        })
        .sum()
}

/// The last line the terminal shows: after the last newline, what follows
/// the last carriage return that has text after it.
pub fn last_shown_line(output: &str) -> String {
    let line = output.trim_end().rsplit('\n').next().unwrap_or("");
    line.rsplit('\r').map(visible).find(|part| !part.trim().is_empty()).unwrap_or_default()
}

/// The frames of the live line: each is drawn as "\r\x1b[J" + line + "\r".
pub fn frames(output: &str) -> Vec<String> {
    output.split("\r\x1b[J").skip(1).filter(|chunk| chunk.ends_with('\r') && !chunk.contains('\n')).map(|chunk| chunk.to_owned()).collect()
}
