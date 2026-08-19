//! Reading a child process's output by deadline instead of by guess.
//!
//! A test that wants the third line a subprocess prints used to sleep for a
//! second and hope. That is wrong twice: it is slow when the line comes back in
//! two milliseconds, and it is flaky when the machine is busy. A pipe has no
//! read timeout, so the shape that does work is a thread that owns the pipe and
//! a channel the test can wait on with a deadline.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Subscribe to a running daemon's log over its control socket.
///
/// The child is returned with it: the caller has to end the subscription, and
/// dropping the `Lines` alone would leave a process reading a socket nobody is
/// listening to.
pub fn start_log_stream(socket: &Path, level: &str, home: &Path) -> (Child, Lines) {
    let mut cmd = Command::new(super::tandem_bin());
    cmd.args([
        "server",
        "logs",
        "--json",
        "--level",
        level,
        "--control-socket",
        socket.to_str().expect("socket path"),
    ]);
    super::isolate_env(&mut cmd, home);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn tandem server logs");
    let lines = Lines::from(child.stdout.take().expect("logs stdout"));
    (child, lines)
}

pub struct Lines {
    rx: Receiver<String>,
    seen: Vec<String>,
}

impl Lines {
    /// Take ownership of a pipe and start draining it.
    ///
    /// Draining matters on its own: a child that fills its pipe buffer blocks,
    /// and a blocked child never prints the line the test is waiting for.
    pub fn from(reader: impl Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            rx,
            seen: Vec::new(),
        }
    }

    /// Wait for the next line that matches, or give up at the deadline.
    ///
    /// Lines that do not match are kept, so a test can assert on everything it
    /// saw when something goes wrong.
    pub fn wait_for(&mut self, timeout: Duration, matches: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if matches(&line) {
                        return Some(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) => return None,
                // The pipe closed: nothing more will ever arrive.
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Take whatever has already arrived, without waiting for more.
    pub fn drain(&mut self) {
        while let Ok(line) = self.rx.try_recv() {
            self.seen.push(line);
        }
    }

    /// Every line read so far, in order. For failure messages.
    pub fn seen(&mut self) -> &[String] {
        self.drain();
        &self.seen
    }

    /// Everything read so far as one block of text. For failure messages.
    pub fn transcript(&mut self) -> String {
        self.seen().join("\n")
    }
}
