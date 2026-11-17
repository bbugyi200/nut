use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{HostError, ProcessOutput, ProcessRequest};

const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn run_bounded(request: &ProcessRequest<'_>) -> Result<ProcessOutput, HostError> {
    if request.inherit_tty {
        return run_inherited(request);
    }
    let mut child = Process(
        Command::new(request.program)
            .args(request.args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| spawn_error(request, error))?,
    );
    let mut stdout_pipe = child
        .0
        .stdout
        .take()
        .ok_or_else(|| HostError::message("missing process stdout"))?;
    let mut stderr_pipe = child
        .0
        .stderr
        .take()
        .ok_or_else(|| HostError::message("missing process stderr"))?;
    let stdout_thread = std::thread::spawn(move || read_bounded(&mut stdout_pipe));
    let stderr_thread = std::thread::spawn(move || read_bounded(&mut stderr_pipe));
    let started = Instant::now();
    let status = loop {
        if started.elapsed() >= request.timeout {
            return Err(timeout_error(request));
        }
        match child.0.try_wait().map_err(|error| {
            HostError::from_io(
                &format!("cannot wait for {}", request.program.to_string_lossy()),
                error,
            )
        })? {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = stdout_thread.join().unwrap_or_else(|_| Vec::new());
    let stderr = stderr_thread.join().unwrap_or_else(|_| Vec::new());
    Ok(ProcessOutput {
        status: status.code(),
        stdout,
        stderr,
    })
}

fn run_inherited(request: &ProcessRequest<'_>) -> Result<ProcessOutput, HostError> {
    let mut child = Process(
        Command::new(request.program)
            .args(request.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| spawn_error(request, error))?,
    );
    let started = Instant::now();
    loop {
        if started.elapsed() >= request.timeout {
            return Err(timeout_error(request));
        }
        match child.0.try_wait().map_err(|error| {
            HostError::from_io(
                &format!("cannot wait for {}", request.program.to_string_lossy()),
                error,
            )
        })? {
            Some(status) => {
                return Ok(ProcessOutput {
                    status: status.code(),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn read_bounded(pipe: &mut impl Read) -> Vec<u8> {
    let mut sink = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let remaining = MAX_CAPTURE_BYTES.saturating_sub(sink.len());
                sink.extend_from_slice(&buffer[..count.min(remaining)]);
                if sink.len() >= MAX_CAPTURE_BYTES {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    sink
}

fn spawn_error(request: &ProcessRequest<'_>, error: std::io::Error) -> HostError {
    if request.secret {
        HostError::message(format!(
            "cannot start {}",
            request.program.to_string_lossy()
        ))
    } else {
        HostError::from_io(
            &format!("cannot run {}", request.program.to_string_lossy()),
            error,
        )
    }
}

fn timeout_error(request: &ProcessRequest<'_>) -> HostError {
    HostError::message(format!("{} timed out", request.program.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Host, SystemHost};
    use std::ffi::{OsStr, OsString};
    use std::time::Duration;

    #[test]
    fn bounded_run_times_out_without_including_output() {
        let request = ProcessRequest {
            program: OsStr::new("sleep"),
            args: &[OsString::from("2")],
            timeout: Duration::from_millis(40),
            secret: true,
            inherit_tty: false,
        };
        let error = SystemHost.run_bounded(&request).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("timed out"), "{text}");
        assert!(!text.contains("sleep:"), "{text}");
    }

    #[test]
    fn bounded_run_captures_status_and_hides_secret_spawn_errors() {
        let request = ProcessRequest {
            program: OsStr::new("true"),
            args: &[],
            timeout: Duration::from_secs(2),
            secret: false,
            inherit_tty: false,
        };
        let output = SystemHost.run_bounded(&request).unwrap();
        assert_eq!(output.status, Some(0));

        let missing = ProcessRequest {
            program: OsStr::new("/no/such/nut-secret-command"),
            args: &[],
            timeout: Duration::from_secs(1),
            secret: true,
            inherit_tty: false,
        };
        let error = SystemHost.run_bounded(&missing).unwrap_err();
        assert_eq!(
            error.to_string(),
            "cannot start /no/such/nut-secret-command"
        );
    }
}
