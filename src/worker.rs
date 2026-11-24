use std::{
    cell::RefCell,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use process_wrap::std::CommandWrap;

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Operação cancelada.")
    }
}

impl std::error::Error for Cancelled {}

type Reporter = Box<dyn Fn(&str, Option<f32>) + Send>;

pub struct Job {
    cancel: Arc<AtomicBool>,
    report: Reporter,
    last_report: RefCell<Option<(String, Instant)>>,
}

impl Job {
    pub fn new(cancel: Arc<AtomicBool>, report: Reporter) -> Self {
        Self {
            cancel,
            report,
            last_report: RefCell::new(None),
        }
    }

    pub fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    pub fn progress(&self, label: &str, fraction: Option<f32>) {
        let mut last = self.last_report.borrow_mut();
        if last.as_ref().is_some_and(|(previous, time)| {
            previous == label && time.elapsed() < Duration::from_millis(100)
        }) {
            return;
        }
        *last = Some((label.to_owned(), Instant::now()));
        (self.report)(
            label,
            fraction
                .filter(|v| v.is_finite())
                .map(|v| v.clamp(0.0, 1.0)),
        );
    }
}

pub fn run(mut command: Command, job: &Job, mut on_line: impl FnMut(&str)) -> Result<String> {
    job.check()?;
    let mut stderr =
        tempfile::tempfile().context("Não foi possível abrir o registro temporário.")?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr.try_clone()?);
    let mut command = CommandWrap::from(command);
    #[cfg(windows)]
    {
        use process_wrap::std::{CreationFlags, JobObject};
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        command
            .wrap(CreationFlags(CREATE_NO_WINDOW))
            .wrap(JobObject);
    }
    let mut child = command
        .spawn()
        .context("Não foi possível iniciar a ferramenta.")?;
    let stdout = child
        .stdout()
        .take()
        .context("Saída da ferramenta indisponível.")?;
    let (send, receive) = mpsc::sync_channel(32);
    thread::spawn(move || {
        for line in BufReader::new(stdout)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if send.send(line).is_err() {
                break;
            }
        }
    });

    let mut output = String::new();
    let mut handle_line = |line: String| {
        on_line(&line);
        if output.len() + line.len() > 1024 * 1024 {
            output.clear();
        }
        output.push_str(&line);
        output.push('\n');
    };
    let tick = Duration::from_millis(80);
    let status = loop {
        if let Err(error) = job.check() {
            child
                .kill()
                .context("Não foi possível cancelar a ferramenta.")?;
            return Err(error);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Err(error) => {
                let _ = child.kill();
                return Err(error).context("Não foi possível acompanhar a ferramenta.");
            }
            Ok(None) => {}
        }
        match receive.recv_timeout(tick) {
            Ok(line) => handle_line(line),
            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(tick),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    drop(child);
    for line in receive {
        handle_line(line);
    }
    job.check()?;
    if !status.success() {
        let start = stderr.metadata()?.len().saturating_sub(6000);
        stderr.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes)?;
        let diagnostic = String::from_utf8_lossy(&bytes);
        let last = diagnostic
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty());
        bail!(
            "{}",
            last.unwrap_or("A ferramenta terminou sem gerar o resultado.")
        );
    }
    Ok(output)
}
