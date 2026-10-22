//! Running helper programs (`git`, `herdr`) from the sampler thread without
//! letting a wedged one stall it: every call has a deadline, after which the
//! child is killed and the caller gets `None`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run `cmd`, returning its stdout if it exits successfully within `timeout`.
pub fn output(mut cmd: Command, timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drain stdout on a thread so a chatty child can't block on a full pipe
    // while we wait for it to exit.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).ok().map(|_| buf)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()??;
    status.success().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_returns_stdout_and_respects_the_deadline() {
        let mut echo = Command::new("echo");
        echo.arg("hi");
        assert_eq!(
            output(echo, Duration::from_secs(2)).as_deref(),
            Some("hi\n")
        );
        let mut slow = Command::new("sleep");
        slow.arg("5");
        let t = Instant::now();
        assert_eq!(output(slow, Duration::from_millis(100)), None);
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "killed at the deadline"
        );
        assert_eq!(output(Command::new("false"), Duration::from_secs(2)), None);
        assert_eq!(
            output(Command::new("/nonexistent/x"), Duration::from_secs(1)),
            None
        );
    }
}
