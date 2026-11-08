use std::{
    io::{self, BufRead, BufReader, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub struct TestProcess {
    child: Child,
    lines: Receiver<io::Result<String>>,
    reader: Option<JoinHandle<()>>,
    output: String,
    reaped: bool,
}

impl TestProcess {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let stdout = child.stdout.take().expect("spawn configured piped stdout");
        let (sender, lines) = mpsc::channel();
        let mut process = Self { child, lines, reader: None, output: String::new(), reaped: false };
        process.reader = Some(thread::Builder::new().name("test-process-stdout".into()).spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let failed = line.is_err();
                if sender.send(line).is_err() || failed {
                    break;
                }
            }
        })?);
        Ok(process)
    }

    pub fn wait_for_stdout(&mut self, marker: &str) -> io::Result<()> {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            let line = self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())).map_err(|error| {
                io::Error::new(
                    match error {
                        mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                        mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::UnexpectedEof,
                    },
                    format!("subprocess did not write {marker:?}: {error}; stdout: {}", self.output),
                )
            })??;
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
            reader.join().map_err(|_| io::Error::other("subprocess stdout reader panicked"))?;
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

    #[test]
    fn child_helper() -> io::Result<()> {
        let Ok(mode) = env::var("RADIA_TEST_PROCESS_MODE") else { return Ok(()) };
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
