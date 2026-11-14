use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    mem,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const READ_BUFFER: usize = 8 * 1024;
const OUTPUT_LIMIT: usize = 1024 * 1024;
const MESSAGE_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureLimit {
    Bytes,
    Messages,
}

impl fmt::Display for CaptureLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes => write!(f, "subprocess stdout exceeded {OUTPUT_LIMIT} bytes"),
            Self::Messages => write!(f, "subprocess stdout exceeded {MESSAGE_LIMIT} lines"),
        }
    }
}

impl Error for CaptureLimit {}

pub struct TestProcess {
    child: Child,
    lines: Receiver<io::Result<String>>,
    reader: Option<JoinHandle<Result<(), CaptureLimit>>>,
    limit: Option<CaptureLimit>,
    output: String,
    reaped: bool,
}

impl TestProcess {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let stdout = child.stdout.take().expect("spawn configured piped stdout");
        let (sender, lines) = mpsc::channel();
        let mut process = Self { child, lines, reader: None, limit: None, output: String::new(), reaped: false };
        process.reader = Some(thread::Builder::new().name("test-process-stdout".into()).spawn(move || read_lines(stdout, &sender))?);
        Ok(process)
    }

    pub fn wait_for_stdout(&mut self, marker: &str) -> io::Result<()> {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            let line = match self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => line?,
                Err(error) => {
                    let kind = match error {
                        mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                        mpsc::RecvTimeoutError::Disconnected => {
                            self.finish_reader()?;
                            if let Some(limit) = self.limit {
                                return Err(io::Error::other(limit));
                            }
                            io::ErrorKind::UnexpectedEof
                        }
                    };
                    return Err(io::Error::new(kind, format!("subprocess did not write {marker:?}: {error}; stdout: {}", self.output)));
                }
            };
            self.output.push_str(&line);
            self.output.push('\n');
            if line == marker {
                return Ok(());
            }
        }
    }

    pub fn write_stdin(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.child.stdin.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "subprocess stdin is closed"))?.write_all(bytes)
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_until(Instant::now() + PROCESS_TIMEOUT)
    }

    pub fn kill(&mut self) -> io::Result<ExitStatus> {
        let status = kill_and_wait(&mut self.child)?;
        self.reaped = true;
        self.child.stdin.take();
        self.finish_reader()?;
        Ok(status)
    }

    pub fn stdout(&self) -> &str {
        &self.output
    }

    fn wait_until(&mut self, deadline: Instant) -> io::Result<ExitStatus> {
        self.child.stdin.take();
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                self.finish_reader()?;
                if let Some(limit) = self.limit {
                    return Err(io::Error::other(limit));
                }
                return Ok(status);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _status = kill_and_wait(&mut self.child)?;
                self.reaped = true;
                self.finish_reader()?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("subprocess did not exit before its deadline; stdout: {}", self.output),
                ));
            }
            thread::sleep(remaining.min(PROCESS_POLL_INTERVAL));
        }
    }

    fn finish_reader(&mut self) -> io::Result<()> {
        if let Some(reader) = self.reader.take() {
            let result = reader.join().map_err(|_| io::Error::other("subprocess stdout reader panicked"))?;
            self.limit = result.err();
        }
        for line in self.lines.try_iter() {
            self.output.push_str(&line?);
            self.output.push('\n');
        }
        Ok(())
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        if !self.reaped {
            match kill_and_wait(&mut self.child) {
                Ok(_) => self.reaped = true,
                Err(error) => eprintln!("failed to kill and reap test subprocess: {error}"),
            }
        }
        self.child.stdin.take();
        if self.reaped
            && let Err(error) = self.finish_reader()
        {
            eprintln!("failed to finish test subprocess stdout: {error}");
        }
    }
}

fn read_lines(mut stdout: impl Read, sender: &Sender<io::Result<String>>) -> Result<(), CaptureLimit> {
    let mut buffer = [0; READ_BUFFER];
    let mut pending = Vec::new();
    let mut bytes = 0;
    let mut messages = 0;
    loop {
        let read = match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                sender.send(Err(error)).ok();
                return Ok(());
            }
        };
        bytes += read;
        if bytes > OUTPUT_LIMIT {
            return Err(CaptureLimit::Bytes);
        }
        for part in buffer[..read].split_inclusive(|&byte| byte == b'\n') {
            pending.extend_from_slice(part);
            if !part.ends_with(b"\n") {
                continue;
            }
            messages += 1;
            if messages > MESSAGE_LIMIT {
                return Err(CaptureLimit::Messages);
            }
            if !send_line(&mut pending, sender) {
                return Ok(());
            }
        }
    }
    if !pending.is_empty() {
        if messages == MESSAGE_LIMIT {
            return Err(CaptureLimit::Messages);
        }
        send_line(&mut pending, sender);
    }
    Ok(())
}

fn send_line(pending: &mut Vec<u8>, sender: &Sender<io::Result<String>>) -> bool {
    let mut line = mem::take(pending);
    if line.ends_with(b"\n") {
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
    }
    let line = String::from_utf8(line).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8"));
    let ok = line.is_ok();
    sender.send(line).is_ok() && ok
}

fn kill_and_wait(child: &mut Child) -> io::Result<ExitStatus> {
    match child.kill() {
        Ok(()) => child.wait(),
        Err(error) => match child.try_wait()? {
            Some(status) => Ok(status),
            None => Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        io::Read,
        net::{TcpListener, TcpStream},
        panic::{self, AssertUnwindSafe},
    };

    use super::*;

    fn helper(mode: &str) -> io::Result<TestProcess> {
        TestProcess::spawn(
            Command::new(env::current_exe()?)
                .args(["--exact", "process::tests::child_helper", "--nocapture"])
                .env("RADIA_TEST_PROCESS_MODE", mode),
        )
    }

    #[test]
    fn early_exit_reports_output() -> io::Result<()> {
        let mut child = helper("exit")?;
        let error = child.wait_for_stdout("ready").expect_err("helper exits without readiness");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(error.to_string().contains("helper exiting"), "{error}");
        assert!(child.wait()?.success());
        Ok(())
    }

    #[test]
    fn unwind_closes_child_connection() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut child = helper(&listener.local_addr()?.to_string())?;
        child.wait_for_stdout("ready")?;
        let (mut peer, _) = listener.accept()?;
        peer.set_read_timeout(Some(PROCESS_TIMEOUT))?;
        let unwound = panic::catch_unwind(AssertUnwindSafe(move || {
            let _child = child;
            panic!("exercise subprocess cleanup during unwind");
        }));
        assert!(unwound.is_err());
        match peer.read(&mut [0]) {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            result => panic!("child connection must close during cleanup: {result:?}"),
        }
        Ok(())
    }

    #[test]
    fn wait_timeout_kills_and_reaps_child() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mode = format!("hold:{}", listener.local_addr()?);
        let mut child = helper(&mode)?;
        child.wait_for_stdout("ready")?;
        let (mut peer, _) = listener.accept()?;
        peer.set_read_timeout(Some(PROCESS_TIMEOUT))?;

        let stdin = child.child.stdin.take().ok_or_else(|| io::Error::other("subprocess stdin is not piped"))?;
        drop(stdin);
        child.wait_for_stdout("stdin closed")?;

        let error = child.wait_until(Instant::now()).expect_err("expired deadline must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(child.stdout().contains("ready\n"), "{}", child.stdout());
        assert!(child.stdout().contains("stdin closed\n"), "{}", child.stdout());
        assert!(error.to_string().contains(child.stdout()), "{error}");
        match peer.read(&mut [0]) {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            result => panic!("child connection must close on timeout: {result:?}"),
        }

        let status = child.wait()?;
        assert!(!status.success(), "timed out child must be reaped after termination");
        Ok(())
    }

    fn limit_of(error: &io::Error) -> Option<CaptureLimit> {
        error.get_ref()?.downcast_ref::<CaptureLimit>().copied()
    }

    #[test]
    fn stdout_line_is_bounded() -> io::Result<()> {
        let mut child = helper("long-line")?;
        let error = child.wait_for_stdout("never").expect_err("an oversized line must not be accepted");
        assert_eq!(limit_of(&error), Some(CaptureLimit::Bytes), "{error}");
        assert!(child.stdout().len() <= OUTPUT_LIMIT, "retained {} bytes", child.stdout().len());
        let error = child.wait().expect_err("wait must report the capture limit");
        assert_eq!(limit_of(&error), Some(CaptureLimit::Bytes), "{error}");
        assert!(child.reaped);
        assert!(child.reader.is_none());
        Ok(())
    }

    #[test]
    fn stdout_capture_is_bounded() -> io::Result<()> {
        for (mode, limit) in [("lines", CaptureLimit::Bytes), ("empty-lines", CaptureLimit::Messages)] {
            let mut child = helper(mode)?;
            let error = child.wait_for_stdout("never").expect_err("flood must not be accepted");
            assert_eq!(limit_of(&error), Some(limit), "{mode}: {error}");
            assert!(child.stdout().len() <= OUTPUT_LIMIT, "{mode}: retained {} bytes", child.stdout().len());
            let error = child.wait().expect_err("wait must report the capture limit");
            assert_eq!(limit_of(&error), Some(limit), "{mode}: {error}");
            assert!(child.reaped, "{mode}");

            let mut child = helper(mode)?;
            let error = child.wait().expect_err("wait without readiness must report the capture limit");
            assert_eq!(limit_of(&error), Some(limit), "{mode}: {error}");
            assert!(child.stdout().len() <= OUTPUT_LIMIT, "{mode}: retained {} bytes", child.stdout().len());
            assert!(child.reaped, "{mode}");
        }
        Ok(())
    }

    #[test]
    fn capture_limit_edges() {
        let long = |len: usize, tail: &str| format!("{}{tail}", "x".repeat(len)).into_bytes();
        let cases = [
            ("exact bytes", long(OUTPUT_LIMIT - 1, "\n"), Ok(()), vec!["x".repeat(OUTPUT_LIMIT - 1)]),
            ("one byte over", long(OUTPUT_LIMIT, "\n"), Err(CaptureLimit::Bytes), vec![]),
            ("exact unterminated", long(OUTPUT_LIMIT, ""), Ok(()), vec!["x".repeat(OUTPUT_LIMIT)]),
            ("exact lines", vec![b'\n'; MESSAGE_LIMIT], Ok(()), vec![String::new(); MESSAGE_LIMIT]),
            ("one line over", vec![b'\n'; MESSAGE_LIMIT + 1], Err(CaptureLimit::Messages), vec![String::new(); MESSAGE_LIMIT]),
            (
                "unterminated over",
                [vec![b'\n'; MESSAGE_LIMIT], b"x".to_vec()].concat(),
                Err(CaptureLimit::Messages),
                vec![String::new(); MESSAGE_LIMIT],
            ),
            ("crlf", b"a\r\nb".to_vec(), Ok(()), vec!["a".into(), "b".into()]),
        ];
        for (name, input, expected, expected_lines) in cases {
            let (sender, lines) = mpsc::channel();
            assert_eq!(read_lines(input.as_slice(), &sender), expected, "{name}");
            drop(sender);
            let lines: Vec<String> = lines.try_iter().collect::<io::Result<_>>().expect("valid UTF-8");
            assert_eq!(lines, expected_lines, "{name}");
        }
    }

    #[test]
    fn invalid_utf8_is_error() {
        let (sender, lines) = mpsc::channel();
        assert_eq!(read_lines(&b"ok\n\xff\nlater\n"[..], &sender), Ok(()));
        drop(sender);
        let results: Vec<_> = lines.try_iter().collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().ok().map(String::as_str), Some("ok"));
        assert_eq!(results[1].as_ref().err().map(io::Error::kind), Some(io::ErrorKind::InvalidData));
    }

    #[test]
    fn limit_kill_reaps() -> io::Result<()> {
        let mut child = helper("lines")?;
        let error = child.wait_for_stdout("never").expect_err("flood must not be accepted");
        assert_eq!(limit_of(&error), Some(CaptureLimit::Bytes), "{error}");
        child.kill()?;
        assert!(child.reaped);
        assert!(child.reader.is_none());
        Ok(())
    }

    #[test]
    fn child_helper() -> io::Result<()> {
        let Ok(mode) = env::var("RADIA_TEST_PROCESS_MODE") else { return Ok(()) };
        let flood = match mode.as_str() {
            "long-line" => Some((vec![b'x'; 64 * 1024], 2 * OUTPUT_LIMIT / (64 * 1024))),
            "lines" => Some(([&[b'x'; 1023][..], b"\n"].concat().repeat(64), 2 * OUTPUT_LIMIT / (64 * 1024))),
            "empty-lines" => Some((vec![b'\n'; 1024], MESSAGE_LIMIT / 1024 + 1)),
            _ => None,
        };
        if let Some((pattern, count)) = flood {
            let mut stdout = io::stdout().lock();
            for _ in 0..count {
                stdout.write_all(&pattern)?;
            }
            return stdout.flush();
        }
        if mode == "exit" {
            println!("helper exiting");
            return Ok(());
        }
        let (address, hold_open) = mode.strip_prefix("hold:").map_or((mode.as_str(), false), |address| (address, true));
        let mut peer = TcpStream::connect(address)?;
        println!("ready");
        io::stdout().flush()?;
        io::stdin().read_line(&mut String::new())?;
        if hold_open {
            println!("stdin closed");
            io::stdout().flush()?;
            let mut byte = [0];
            while peer.read(&mut byte)? != 0 {}
        }
        Ok(())
    }
}
