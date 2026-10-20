// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::error::{Error, IoResultExt, Result};

/// How often the watcher re-checks child state while waiting for a marker.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Grace period for the reader thread to observe EOF after the child was
/// terminated. Orphaned descendants that inherited the stdout pipe can hold
/// it open arbitrarily long, so the watcher gives up rather than blocking.
const READER_FLUSH_GRACE: Duration = Duration::from_secs(1);

/// Classification of a matched output marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanHit {
    /// The run reached its success checkpoint.
    Success,
    /// An immediate-failure marker matched; the payload is the marker text.
    Failure(String),
}

/// Auto-termination rule for a watched tee'd child run.
///
/// The watcher scans the mirrored child output and terminates the child as
/// soon as the success marker or any failure marker appears, with a backstop
/// timeout for output that never matches. Automation runs (for example a QEMU
/// boot check) therefore exit deterministically on observable output instead
/// of guessing a wall-clock timeout.
#[derive(Debug, Clone)]
pub(crate) struct ExitWatch {
    success_marker: String,
    failure_markers: Vec<String>,
    timeout: Duration,
}

impl ExitWatch {
    pub(crate) fn new(
        success_marker: impl Into<String>,
        failure_markers: Vec<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            success_marker: success_marker.into(),
            failure_markers,
            timeout,
        }
    }

    pub(crate) fn success_marker(&self) -> &str {
        &self.success_marker
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Length of the longest marker; drives the carry-over window between
    /// consecutive output chunks so markers split across reads still match.
    fn marker_window(&self) -> usize {
        self.failure_markers
            .iter()
            .map(String::len)
            .chain([self.success_marker.len()])
            .max()
            .unwrap_or(0)
    }

    /// Feeds one output chunk to the watcher and reports the first match.
    ///
    /// `tail` carries over the bytes that could still complete a marker in
    /// the next chunk; it must start empty and is kept bounded by this method.
    fn scan(&self, tail: &mut Vec<u8>, chunk: &[u8]) -> Option<ScanHit> {
        let mut combined = std::mem::take(tail);
        combined.extend_from_slice(chunk);
        let keep = self.marker_window().saturating_sub(1).min(combined.len());
        let hit = self.scan_buffer(&combined);
        tail.extend_from_slice(&combined[combined.len() - keep..]);
        hit
    }

    fn scan_buffer(&self, bytes: &[u8]) -> Option<ScanHit> {
        if contains_marker(bytes, &self.success_marker) {
            return Some(ScanHit::Success);
        }
        self.failure_markers
            .iter()
            .find(|marker| contains_marker(bytes, marker))
            .map(|marker| ScanHit::Failure((*marker).clone()))
    }
}

/// Reports whether `bytes` contains the non-empty `marker`.
fn contains_marker(bytes: &[u8], marker: &str) -> bool {
    !marker.is_empty()
        && marker.len() <= bytes.len()
        && bytes
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
}

/// Outcome of a tee'd run with an optional [`ExitWatch`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TeeOutcome {
    /// The child exited on its own with this status.
    Exit(ExitStatus),
    /// The success marker was seen and the child was terminated.
    MarkerReached,
    /// A failure marker was seen and the child was terminated.
    MarkerFailed(String),
    /// No marker appeared within the watch timeout; the child was terminated.
    TimedOut,
}

pub(crate) struct Process {
    command: Command,
    dry_run: bool,
    verbosity: u8,
    stdout_path: Option<PathBuf>,
}

impl Process {
    pub(crate) fn new(program: impl Into<OsString>, dry_run: bool, verbosity: u8) -> Self {
        Self {
            command: Command::new(program.into()),
            dry_run,
            verbosity,
            stdout_path: None,
        }
    }

    pub(crate) fn current_dir(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.command.current_dir(path);
        self
    }

    pub(crate) fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.command.arg(arg.into());
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.command.args(args.into_iter().map(Into::into));
        self
    }

    pub(crate) fn env(
        &mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> &mut Self {
        self.command.env(key.into(), value.into());
        self
    }

    pub(crate) fn env_remove(&mut self, key: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.command.env_remove(key);
        self
    }

    pub(crate) fn stdout_to_file(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.stdout_path = Some(path.into());
        self
    }

    /// Renders the command line wrapped across multiple lines for display.
    ///
    /// Each `-flag value` group starts a new line terminated by a shell line
    /// continuation (`\`), so the output stays readable on a narrow terminal
    /// while remaining copy-pasteable into a shell.
    pub(crate) fn command_lines(&self) -> String {
        let parts = self.command_parts();
        let mut lines: Vec<String> = Vec::new();
        let mut current: Vec<String> = Vec::new();
        for part in parts {
            let is_flag = part.starts_with('-');
            if is_flag && !current.is_empty() {
                lines.push(current.join(" "));
                current.clear();
            }
            current.push(part);
        }
        if !current.is_empty() {
            lines.push(current.join(" "));
        }
        lines.join(" \\\n  ")
    }

    /// Returns the program followed by shell-quoted arguments, plus an optional
    /// stdout redirection suffix.
    fn command_parts(&self) -> Vec<String> {
        let program = self.command.get_program().to_string_lossy();
        let mut parts: Vec<String> = std::iter::once(program.into_owned())
            .chain(
                self.command
                    .get_args()
                    .map(|arg| shell_quote(arg.to_string_lossy().as_ref())),
            )
            .collect();
        if let Some(path) = &self.stdout_path {
            parts.push(format!("> {}", shell_quote(&path.display().to_string())));
        }
        parts
    }

    /// Run the command while mirroring stdout to the terminal and to `path`.
    ///
    /// The child's stdout is piped and forwarded by a reader thread to both
    /// the parent's stdout and the file, so the run stays live on the
    /// terminal while the full output is preserved for post-processing
    /// (e.g. automatic panic backtrace symbolication). stderr and stdin stay
    /// inherited so interactive QEMU sessions keep working.
    ///
    /// With `watch`, the reader thread additionally scans the output for the
    /// watcher's markers; the child is terminated as soon as a marker matches
    /// (or the backstop timeout expires) and the outcome is reported instead
    /// of the child's own exit status.
    pub(crate) fn run_tee(
        &mut self,
        path: impl Into<PathBuf>,
        watch: Option<&ExitWatch>,
    ) -> Result<TeeOutcome> {
        let output_path = path.into();
        if self.verbosity > 0 || self.dry_run {
            println!("+ {:#?}", self.command);
            println!("  stdout tee -> {}", output_path.display());
        }
        if self.dry_run {
            return Ok(TeeOutcome::Exit(success_exit_status()));
        }

        let file = File::create(&output_path).with_path(&output_path)?;
        self.command.stdout(Stdio::piped());
        let program = self.command.get_program().to_string_lossy().into_owned();
        let mut child = self.command.spawn().map_err(|source| Error::Spawn {
            program: program.clone(),
            source,
        })?;
        let mut pipe = child
            .stdout
            .take()
            .ok_or_else(|| Error::Message(format!("failed to capture {program} stdout")))?;
        let tee_file = Arc::new(Mutex::new(file));
        let reader_tee_file = Arc::clone(&tee_file);
        // Shared slot holding the first marker hit, if any. The reader thread
        // keeps mirroring output after a hit; the main thread terminates the
        // child as soon as the slot is filled.
        let hit_slot: Arc<Mutex<Option<ScanHit>>> = Arc::new(Mutex::new(None));
        let reader_hit_slot = Arc::clone(&hit_slot);
        let reader_watch = watch.cloned();
        let (reader_done_tx, reader_done_rx) = std::sync::mpsc::channel::<()>();
        // The reader thread is detached on purpose: the watcher abandons it
        // after the grace period if orphaned descendants keep the pipe open.
        std::thread::spawn(move || {
            let mut carry_over: Vec<u8> = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => {
                        // Lock stdout only for this write. Holding the lock
                        // across the blocking read below would stall every
                        // other printer, including the caller after it has
                        // given up on this thread.
                        {
                            let mut stdout = io::stdout().lock();
                            let _ = stdout.write_all(&buffer[..length]);
                            let _ = stdout.flush();
                        }
                        if let Ok(mut file) = reader_tee_file.lock() {
                            let _ = file.write_all(&buffer[..length]);
                        }
                        if let Some(watch) = &reader_watch
                            && let Ok(mut slot) = reader_hit_slot.lock()
                            && slot.is_none()
                            && let Some(hit) =
                                watch.scan(&mut carry_over, &buffer[..length])
                            {
                                *slot = Some(hit);
                            }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = reader_done_tx.send(());
        });

        let outcome = match watch {
            None => {
                // Interactive flow: wait for the child, then for the reader
                // to drain the pipe, exactly like the original tee run.
                let status = match child.wait() {
                    Ok(status) => status,
                    Err(source) => {
                        let _ = reader_done_rx.recv();
                        return Err(Error::Message(format!(
                            "failed to wait for {program}: {source}"
                        )));
                    }
                };
                let _ = reader_done_rx.recv();
                TeeOutcome::Exit(status)
            }
            Some(watch) => {
                let deadline = Instant::now() + watch.timeout();
                loop {
                    // Marker hits win over the child's own exit status:
                    // output the child already flushed must still be judged
                    // even when its exit became waitable in this poll.
                    let pending = hit_slot.lock().ok().and_then(|slot| slot.clone());
                    if let Some(hit) = pending {
                        terminate(&mut child);
                        let _ = reader_done_rx.recv_timeout(READER_FLUSH_GRACE);
                        break marker_outcome(hit);
                    }
                    let exited = match child.try_wait() {
                        Ok(Some(status)) => Some(status),
                        Ok(None) => None,
                        Err(source) => {
                            terminate(&mut child);
                            let _ = reader_done_rx.recv_timeout(READER_FLUSH_GRACE);
                            return Err(Error::Message(format!(
                                "failed to wait for {program}: {source}"
                            )));
                        }
                    };
                    if let Some(status) = exited {
                        // The child exited on its own. Give the reader a
                        // short, bounded grace period to scan output the
                        // child flushed before exiting; a marker observed
                        // there still decides the outcome.
                        let _ = reader_done_rx.recv_timeout(READER_FLUSH_GRACE);
                        let pending = hit_slot.lock().ok().and_then(|slot| slot.clone());
                        if let Some(hit) = pending {
                            break marker_outcome(hit);
                        }
                        break TeeOutcome::Exit(status);
                    }
                    if Instant::now() >= deadline {
                        terminate(&mut child);
                        // The reader may have scanned a marker during the
                        // grace window even though the poll saw an empty
                        // slot right at the deadline; judge it like every
                        // other exit path instead of blindly reporting
                        // TimedOut.
                        let _ = reader_done_rx.recv_timeout(READER_FLUSH_GRACE);
                        let pending = hit_slot.lock().ok().and_then(|slot| slot.clone());
                        if let Some(hit) = pending {
                            break marker_outcome(hit);
                        }
                        break TeeOutcome::TimedOut;
                    }
                    std::thread::sleep(WAIT_POLL_INTERVAL);
                }
            }
        };
        Ok(outcome)
    }

    pub(crate) fn run(&mut self) -> Result<()> {
        if self.verbosity > 0 || self.dry_run {
            // Show the command once, in shell form, so it can be copy-pasted.
            println!("{}", self.command_lines());
            if let Some(path) = &self.stdout_path {
                println!("  stdout -> {}", path.display());
            }
        }
        if self.dry_run {
            return Ok(());
        }

        let pending_stdout = if let Some(path) = &self.stdout_path {
            let pending_path = pending_output_path(path)?;
            let output = File::create(&pending_path).with_path(path)?;
            self.command.stdout(Stdio::from(output));
            Some((pending_path, path.clone()))
        } else {
            None
        };

        let program = self.command.get_program().to_string_lossy().into_owned();
        let status = match self.command.status() {
            Ok(status) => status,
            Err(source) => {
                self.command.stdout(Stdio::inherit());
                remove_pending_output(pending_stdout.as_ref());
                return Err(Error::Spawn {
                    program: program.clone(),
                    source,
                });
            }
        };
        self.command.stdout(Stdio::inherit());
        if !status.success() {
            remove_pending_output(pending_stdout.as_ref());
            return Err(Error::CommandFailed { program, status });
        }
        if let Some((pending_path, output_path)) = pending_stdout {
            match fs::remove_file(&output_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    let _ = fs::remove_file(&pending_path);
                    return Err(Error::Io {
                        path: output_path,
                        source,
                    });
                }
            }
            if let Err(source) = fs::rename(&pending_path, &output_path) {
                let _ = fs::remove_file(&pending_path);
                return Err(Error::Io {
                    path: output_path,
                    source,
                });
            }
        }
        Ok(())
    }
}

/// Maps a matched marker to the reported outcome.
fn marker_outcome(hit: ScanHit) -> TeeOutcome {
    match hit {
        ScanHit::Success => TeeOutcome::MarkerReached,
        ScanHit::Failure(marker) => TeeOutcome::MarkerFailed(marker),
    }
}

/// Terminates a child and reaps it, tolerating an already-exited child.
fn terminate(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// A synthetic successful exit status for dry-run outcomes.
fn success_exit_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    ExitStatus::from_raw(0)
}

fn pending_output_path(output_path: &Path) -> Result<PathBuf> {
    let file_name = output_path.file_name().ok_or_else(|| {
        Error::Message(format!(
            "stdout output path has no file name: {}",
            output_path.display()
        ))
    })?;
    let mut pending_name = file_name.to_os_string();
    pending_name.push(format!(".tmp.{}", std::process::id()));
    Ok(output_path.with_file_name(pending_name))
}

/// Quotes a single command-line argument for shell display.
///
/// Arguments without whitespace or shell metacharacters are returned as-is;
/// anything else is wrapped in single quotes (with embedded single quotes
/// escaped via the standard `'\''` idiom).
fn shell_quote(arg: &str) -> String {
    let needs_quoting = arg.is_empty()
        || arg.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '\'' | '"' | '\\' | '$' | '`' | '|' | '&' | ';' | '<' | '>' | '(' | ')'
                        | '*' | '?' | '[' | ']' | '~' | '#' | '!' | '^' | '{' | '}' | '=' | ':'
                        | '%' | ',' | '+' | '@'
                )
        });
    if !needs_quoting {
        return arg.to_string();
    }
    let escaped = arg.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn remove_pending_output(pending_stdout: Option<&(PathBuf, PathBuf)>) {
    if let Some((pending_path, _)) = pending_stdout {
        let _ = fs::remove_file(pending_path);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{Duration, Instant},
    };

    use super::{Process, ScanHit, TeeOutcome, pending_output_path, shell_quote, ExitWatch};

    fn output_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("xkmake-process-{}-{name}", std::process::id()))
    }

    #[test]
    fn exit_watch_matches_success_marker_across_chunks() {
        let watch = ExitWatch::new("boot complete", vec![], Duration::from_secs(1));
        let mut tail = Vec::new();
        assert_eq!(watch.scan(&mut tail, b"step 1 done; boot "), None);
        // The marker is split across the two chunks.
        assert_eq!(
            watch.scan(&mut tail, b"complete"),
            Some(ScanHit::Success),
            "carry-over bytes must let a split marker match"
        );
    }

    #[test]
    fn exit_watch_reports_failure_marker_and_keeps_tail_bounded() {
        let watch = ExitWatch::new(
            "welcome",
            vec!["panicked at".to_string()],
            Duration::from_secs(1),
        );
        let mut tail = Vec::new();
        assert_eq!(watch.scan(&mut tail, b"all good"), None);
        assert!(
            tail.len() <= watch.marker_window(),
            "carry-over tail must stay bounded by the marker window"
        );
        assert_eq!(
            watch.scan(&mut tail, b"oops, panicked at src/main.rs:1"),
            Some(ScanHit::Failure("panicked at".to_string())),
        );
    }

    #[test]
    fn exit_watch_success_marker_wins_over_failure_markers() {
        let watch = ExitWatch::new("done", vec!["done late".to_string()], Duration::from_secs(1));
        let mut tail = Vec::new();
        assert_eq!(watch.scan(&mut tail, b"all done"), Some(ScanHit::Success));
    }

    #[test]
    fn exit_watch_ignores_empty_markers() {
        let watch = ExitWatch::new("", vec![String::new()], Duration::from_secs(1));
        let mut tail = Vec::new();
        assert_eq!(watch.scan(&mut tail, b"anything"), None);
    }

    #[test]
    fn run_tee_terminates_on_success_marker_before_late_output() {
        let output = output_path("watch-success.log");
        let watch = ExitWatch::new("MARKER_HIT", vec![], Duration::from_secs(30));
        let outcome = Process::new("sh", false, 0)
            .arg("-c")
            // The second marker would only be printed if the child were
            // still alive after the sleep; its absence from the log is an
            // observable-output proof of early termination, independent of
            // wall-clock speed.
            .arg("/bin/echo MARKER_HIT; sleep 3; /bin/echo AFTER_SLEEP")
            .run_tee(&output, Some(&watch))
            .unwrap();

        assert_eq!(outcome, TeeOutcome::MarkerReached);
        let log = fs::read_to_string(&output).unwrap();
        assert!(log.contains("MARKER_HIT"));
        assert!(
            !log.contains("AFTER_SLEEP"),
            "the watcher must terminate the child before it prints the late marker"
        );
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn run_tee_judges_marker_output_flushed_by_an_exiting_child() {
        let output = output_path("watch-exit-race.log");
        let watch = ExitWatch::new("MARKER_HIT", vec![], Duration::from_secs(30));
        // The child flushes the marker and exits immediately; whether the
        // watcher observes the marker before or after the exit becomes
        // waitable is a scheduling race. Both outcomes must report the
        // marker, never the bare exit status.
        let outcome = Process::new("sh", false, 0)
            .arg("-c")
            .arg("/bin/echo MARKER_HIT")
            .run_tee(&output, Some(&watch))
            .unwrap();

        assert_eq!(outcome, TeeOutcome::MarkerReached);
        assert!(fs::read_to_string(&output).unwrap().contains("MARKER_HIT"));
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn run_tee_times_out_without_markers() {
        let output = output_path("watch-timeout.log");
        let watch = ExitWatch::new("never printed", vec![], Duration::from_millis(500));
        let started = Instant::now();
        let outcome = Process::new("sleep", false, 0)
            .arg("5")
            .run_tee(&output, Some(&watch))
            .unwrap();

        assert_eq!(outcome, TeeOutcome::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the backstop must stop the long-sleeping child well before it exits"
        );
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn run_tee_reports_normal_exit_without_watch() {
        let output = output_path("watch-none.log");
        let outcome = Process::new("true", false, 0)
            .run_tee(&output, None)
            .unwrap();
        let TeeOutcome::Exit(status) = outcome else {
            panic!("unwatched runs must report the child's own exit status");
        };
        assert!(status.success());
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn shell_quote_leaves_safe_args_unquoted() {
        assert_eq!(shell_quote("qemu-system-aarch64"), "qemu-system-aarch64");
        assert_eq!(shell_quote("-m"), "-m");
        assert_eq!(shell_quote("2G"), "2G");
    }

    #[test]
    fn shell_quote_wraps_args_with_whitespace_or_metacharacters() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("file with space.img"), "'file with space.img'");
        // An empty argument must be quoted so it is not dropped.
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_wraps_posix_metacharacters() {
        // Each of these is a POSIX shell metacharacter that must trigger
        // quoting so the rendered command line stays copy-pasteable even when
        // a workspace or image path contains one.
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote("a(b)c"), "'a(b)c'");
        assert_eq!(shell_quote("*.rs"), "'*.rs'");
        assert_eq!(shell_quote("a?b"), "'a?b'");
        assert_eq!(shell_quote("a[b]c"), "'a[b]c'");
        assert_eq!(shell_quote("~root"), "'~root'");
        assert_eq!(shell_quote("a#b"), "'a#b'");
        assert_eq!(shell_quote("a!b"), "'a!b'");
        assert_eq!(shell_quote("{a,b}"), "'{a,b}'");
        assert_eq!(shell_quote("k=v"), "'k=v'");
        assert_eq!(shell_quote("a:b"), "'a:b'");
    }

    #[test]
    fn shell_quote_leaves_dotted_identifiers_unquoted() {
        // Plain flags and dotted identifiers (version numbers, dotted paths)
        // must still pass through untouched — only metacharacters trigger
        // quoting, and `.` / `_` are not among them.
        assert_eq!(shell_quote("qemu-system-x86_64"), "qemu-system-x86_64");
        assert_eq!(shell_quote("--machine"), "--machine");
        assert_eq!(shell_quote("2.5.1"), "2.5.1");
    }

    #[test]
    fn command_lines_quotes_stdout_redirection_path() {
        let mut process = Process::new("qemu-system-x86_64", false, 0);
        process
            .arg("-m")
            .arg("2G")
            .stdout_to_file("/tmp/xkmake output.log");
        // The redirection target contains a space and must be shell-quoted so
        // the rendered line stays copy-pasteable into a shell.
        assert_eq!(
            process.command_lines(),
            "qemu-system-x86_64 \\\n  -m 2G > '/tmp/xkmake output.log'"
        );
    }

    #[test]
    fn successful_stdout_is_promoted_to_the_requested_path() {
        let output = output_path("success.txt");
        let _ = fs::remove_file(&output);

        Process::new("rustc", false, 0)
            .arg("--version")
            .stdout_to_file(&output)
            .run()
            .unwrap();

        assert!(fs::read_to_string(&output).unwrap().starts_with("rustc "));
        assert!(!pending_output_path(&output).unwrap().exists());
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn command_lines_wraps_each_flag_group_with_continuation() {
        let mut process = Process::new("qemu-system-aarch64", false, 0);
        process
            .arg("-m")
            .arg("2G")
            .arg("-smp")
            .arg("4")
            .arg("-kernel")
            .arg("kernel.bin");
        // The program name starts the first line; each `-flag value` group then
        // breaks onto its own line, joined by shell continuations so the whole
        // block stays copy-pasteable.
        assert_eq!(
            process.command_lines(),
            "qemu-system-aarch64 \\\n  -m 2G \\\n  -smp 4 \\\n  -kernel kernel.bin"
        );
    }

    #[test]
    fn failed_command_removes_pending_stdout() {
        let output = output_path("failure.txt");
        let pending = pending_output_path(&output).unwrap();
        let _ = fs::remove_file(&output);
        let _ = fs::remove_file(&pending);

        let result = Process::new("false", false, 0)
            .stdout_to_file(&output)
            .run();

        assert!(result.is_err());
        assert!(!output.exists());
        assert!(!pending.exists());
    }
}
