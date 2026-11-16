use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

use super::{HostError, ProcessOutput};

pub(super) const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;

enum Event {
    Line(bool, Vec<u8>),
    Error,
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_pipe(pipe: impl Read, stderr: bool, sender: SyncSender<Event>) {
    let mut reader = BufReader::new(pipe);
    loop {
        let mut line = Vec::new();
        match reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => return,
            Ok(_) if line.len() <= MAX_LINE_BYTES => {
                if sender.send(Event::Line(stderr, line)).is_err() {
                    return;
                }
            }
            _ => {
                let _ = sender.send(Event::Error);
                return;
            }
        }
    }
}

pub(super) fn run(
    program: &OsStr,
    args: &[OsString],
    timeout: Option<Duration>,
    line: &mut dyn FnMut(&[u8]) -> Result<(), HostError>,
) -> Result<ProcessOutput, HostError> {
    let mut child = Process(
        Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| HostError::from_io("cannot start journal reader", error))?,
    );
    let stdout = child
        .0
        .stdout
        .take()
        .ok_or_else(|| HostError::message("missing process stdout"))?;
    let stderr = child
        .0
        .stderr
        .take()
        .ok_or_else(|| HostError::message("missing process stderr"))?;
    let (sender, receiver) = mpsc::sync_channel(8);
    let error_sender = sender.clone();
    std::thread::spawn(move || read_pipe(stdout, false, sender));
    std::thread::spawn(move || read_pipe(stderr, true, error_sender));
    let started = Instant::now();
    let mut errors = Vec::new();
    loop {
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            return Err(HostError::message("journal reader timed out"));
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Line(false, bytes)) => line(&bytes)?,
            Ok(Event::Line(true, bytes)) => {
                let available = MAX_STDERR_BYTES.saturating_sub(errors.len());
                errors.extend_from_slice(&bytes[..available.min(bytes.len())]);
            }
            Ok(Event::Error) => {
                return Err(HostError::message(
                    "cannot read process output or line exceeds 64 KiB",
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(status) = child
                    .0
                    .try_wait()
                    .map_err(|error| HostError::from_io("cannot wait for journal reader", error))?
                {
                    return Ok(ProcessOutput {
                        status: status.code(),
                        stdout: Vec::new(),
                        stderr: errors,
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_lines_and_times_out_quiet_processes() {
        let error = run(
            OsStr::new("sleep"),
            &["2".into()],
            Some(Duration::from_millis(30)),
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let (sender, receiver) = mpsc::sync_channel(1);
        read_pipe(&vec![b'x'; MAX_LINE_BYTES + 1][..], false, sender);
        assert!(matches!(receiver.recv().unwrap(), Event::Error));
    }

    #[test]
    fn streams_lines_without_collecting_stdout() {
        let mut lines = Vec::new();
        let output = run(
            OsStr::new("printf"),
            &["one\\ntwo\\n".into()],
            Some(Duration::from_secs(2)),
            &mut |line| {
                lines.push(line.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(lines, [b"one\n".to_vec(), b"two\n".to_vec()]);
        assert!(output.stdout.is_empty());
        assert_eq!(output.status, Some(0));
    }
}
