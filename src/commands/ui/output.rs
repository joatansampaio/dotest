use crate::core::executor::build_test_command;
use anyhow::Result;
use std::io::Read;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;

use super::config::{RunConfig, Verbosity};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub(super) enum OutputEvent {
    Line(String),
    Finished(Option<i32>),
}

pub(super) fn spawn_test_run(
    filter: Option<String>,
    config: &RunConfig,
) -> Result<(mpsc::Receiver<OutputEvent>, u32)> {
    let (tx, rx) = mpsc::channel();

    let filter_arg = match filter.as_deref() {
        Some("") | None => None,
        Some(f) if f.len() > 31000 => {
            let _ = tx.send(OutputEvent::Line(
                "⚠ Filter too long for Windows. Running ALL tests instead.".to_string(),
            ));
            let _ = tx.send(OutputEvent::Line(String::new()));
            None
        }
        _ => filter,
    };

    let mut cmd = build_test_command(filter_arg, config.no_build, config.no_restore);
    cmd.arg("-v").arg("m");

    match config.verbosity {
        Verbosity::Minimal => {
            cmd.arg("--logger").arg("console");
        }
        Verbosity::Normal => {
            cmd.arg("--logger").arg("console;verbosity=normal");
        }
        Verbosity::Detailed => {
            cmd.arg("--logger").arg("console;verbosity=detailed");
        }
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let tx2 = tx.clone();
    let tx3 = tx.clone();

    thread::spawn(move || pump_stream_lines(stdout, tx));
    thread::spawn(move || pump_stream_lines(stderr, tx2));
    thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code());
        let _ = tx3.send(OutputEvent::Finished(code));
    });

    Ok((rx, pid))
}

// Reads a process stream as raw bytes and emits UI lines on both '\n' and '\r'.
// We use this instead of `BufRead::lines()` because test runner/logging output
// can contain carriage-return updates and partial chunks that are not reliably
// surfaced with the previous technique of using .lines().flatten().
fn pump_stream_lines<R: Read>(mut stream: R, tx: mpsc::Sender<OutputEvent>) {
    const READ_BUF_SIZE: usize = 4096;
    let mut buf = [0_u8; READ_BUF_SIZE];
    let mut pending: Vec<u8> = Vec::new();
    let mut prev_was_cr = false;
    let emit_pending = |pending: &mut Vec<u8>, tx: &mpsc::Sender<OutputEvent>| {
        let line = String::from_utf8_lossy(pending).into_owned();
        pending.clear();
        tx.send(OutputEvent::Line(line)).is_ok()
    };

    loop {
        let read = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        for &byte in &buf[..read] {
            match byte {
                b'\r' => {
                    if !emit_pending(&mut pending, &tx) {
                        return;
                    }
                    prev_was_cr = true;
                }
                b'\n' => {
                    if prev_was_cr {
                        prev_was_cr = false;
                        continue;
                    }

                    if !emit_pending(&mut pending, &tx) {
                        return;
                    }
                }
                _ => {
                    pending.push(byte);
                    prev_was_cr = false;
                }
            }
        }
    }

    if !pending.is_empty() {
        let _ = emit_pending(&mut pending, &tx);
    }
}

pub(super) fn kill_process(pid: u32) {
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        let _ = cmd.spawn();
    }
    #[cfg(not(windows))]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
}
