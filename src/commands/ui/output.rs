use anyhow::Result;
use crate::core::executor::build_test_command;
use std::io::{self, BufRead};
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

pub(super) fn spawn_test_run(filter: Option<String>, config: &RunConfig) -> Result<(mpsc::Receiver<OutputEvent>, u32)> {
    let (tx, rx) = mpsc::channel();

    let filter_arg = match filter.as_deref() {
        Some("") | None => None,
        Some(f) if f.len() > 31000 => {
            let _ = tx.send(OutputEvent::Line(
                "⚠ Filter too long for Windows. Running ALL tests instead.".to_string()
            ));
            let _ = tx.send(OutputEvent::Line(String::new()));
            None
        }
        _ => filter,
    };

    let mut cmd = build_test_command(filter_arg, config.no_build);
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

    thread::spawn(move || {
        for line in io::BufReader::new(stdout).lines().flatten() {
            if tx.send(OutputEvent::Line(line)).is_err() { break; }
        }
    });
    thread::spawn(move || {
        for line in io::BufReader::new(stderr).lines().flatten() {
            if tx2.send(OutputEvent::Line(line)).is_err() { break; }
        }
    });
    thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code());
        let _ = tx3.send(OutputEvent::Finished(code));
    });

    Ok((rx, pid))
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
        unsafe { libc::kill(pid as i32, libc::SIGTERM); }
    }
}
