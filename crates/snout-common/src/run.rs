//! The single seam between snout and every external process it drives — nix,
//! nixos-rebuild, git, sudo, pkill, systemctl.
//!
//! Everything goes through it so the logic above can be tested without a NixOS
//! system underneath: tests swap in [`Runner::fake`] and assert on the argv that
//! would have been executed. Nothing in snout may reach for `std::process` or
//! `tokio::process` directly.

use std::collections::HashMap;
use std::fmt;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Where a stream should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sink {
    /// Captured into the [`Output`].
    #[default]
    Capture,
    /// Handed to the terminal. `sudo nixos-rebuild switch` needs this: its
    /// password prompt wants the tty.
    Inherit,
    /// Discarded.
    Null,
}

impl Sink {
    fn to_stdio(self) -> Stdio {
        match self {
            Sink::Capture => Stdio::piped(),
            Sink::Inherit => Stdio::inherit(),
            Sink::Null => Stdio::null(),
        }
    }
}

/// One process to run. `args[0]` is the program; it is resolved on PATH.
#[derive(Clone, Default)]
pub struct Cmd {
    pub args: Vec<String>,
    pub stdout: Sink,
    pub stderr: Sink,
    /// Attach the parent's stdin, for commands that prompt.
    pub stdin_inherit: bool,
}

impl Cmd {
    pub fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Cmd {
            args: args.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }

    /// Hands stdout, stderr and stdin to the terminal — what a command the user
    /// is watching needs.
    pub fn passthrough(mut self) -> Self {
        self.stdout = Sink::Inherit;
        self.stderr = Sink::Inherit;
        self.stdin_inherit = true;
        self
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.args.join(" "))
    }
}

/// What a finished process produced. `stdout` and `stderr` are empty when the
/// corresponding [`Sink`] was not `Capture`.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

impl Output {
    /// Captured stdout with trailing newlines removed, which is what almost
    /// every caller of a one-line command actually wants.
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout)
            .trim_end_matches('\n')
            .to_string()
    }
}

/// A process that ran to completion with a non-zero status, or failed to start.
#[derive(Debug)]
pub enum RunError {
    /// Ran, and exited non-zero.
    Exit {
        cmd: String,
        code: i32,
        stderr: String,
    },
    /// Never ran: no such program, no permission, or the command was empty.
    Spawn { cmd: String, source: std::io::Error },
}

impl RunError {
    /// The exit status carried by this error, or `None` when the process never
    /// ran. Callers that tolerate a particular non-zero status use this rather
    /// than ignoring the error wholesale.
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            RunError::Exit { code, .. } => Some(*code),
            RunError::Spawn { .. } => None,
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Exit { cmd, code, stderr } if !stderr.is_empty() => {
                write!(f, "{cmd}: exit {code}: {}", last_line(stderr))
            }
            RunError::Exit { cmd, code, .. } => write!(f, "{cmd}: exit {code}"),
            RunError::Spawn { cmd, source } => write!(f, "{cmd}: {source}"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunError::Spawn { source, .. } => Some(source),
            RunError::Exit { .. } => None,
        }
    }
}

/// The one line of stderr worth quoting in a failure message.
///
/// The last `error:` line for preference: nix follows a failed fetch with the
/// server's entire response, so the literally-last line is as likely to be
/// `</html>` as it is to be the diagnosis. Falls back to the last line with
/// anything on it, for the commands that do not use nix's prefixes at all.
fn last_line(s: &str) -> &str {
    let lines = || s.lines().map(str::trim_end).filter(|l| !l.is_empty());
    lines()
        .rfind(|l| l.trim_start().starts_with("error:"))
        .or_else(|| lines().next_back())
        .unwrap_or("")
}

/// Executes processes.
///
/// An enum rather than a trait: there are exactly two implementations, it needs
/// no dynamic dispatch, and it keeps `async fn` usable without boxing every
/// future.
#[derive(Clone)]
pub enum Runner {
    /// The real one.
    Exec,
    /// The test one. See [`Fake`].
    Fake(Arc<Fake>),
}

impl Runner {
    pub fn fake() -> Runner {
        Runner::Fake(Arc::new(Fake::new()))
    }

    /// The [`Fake`] behind this runner, for registering handlers and asserting
    /// on calls. Panics on a real runner — only a test ever asks.
    pub fn as_fake(&self) -> &Fake {
        match self {
            Runner::Fake(f) => f,
            Runner::Exec => panic!("as_fake() on a real runner"),
        }
    }

    /// Runs the command and waits for it. A non-zero exit is a
    /// [`RunError::Exit`], with the [`Output`] still returned alongside.
    pub async fn run(&self, c: &Cmd) -> (Output, Result<(), RunError>) {
        self.run_streaming(c, None::<fn(&str)>).await
    }

    /// Runs the command, delivering each whole line of stderr to `on_line` as it
    /// arrives, in addition to wherever stderr is otherwise going. This is how a
    /// long nix build reports progress while it is still running.
    pub async fn run_streaming<F>(
        &self,
        c: &Cmd,
        on_line: Option<F>,
    ) -> (Output, Result<(), RunError>)
    where
        F: FnMut(&str),
    {
        match self {
            Runner::Exec => exec_run(c, on_line).await,
            Runner::Fake(f) => f.run(c, on_line),
        }
    }

    /// Reports whether a program exists on PATH, and where.
    pub fn look_path(&self, file: &str) -> Option<std::path::PathBuf> {
        match self {
            Runner::Exec => look_path(file),
            Runner::Fake(f) => {
                if f.missing.lock().unwrap().iter().any(|m| m == file) {
                    None
                } else {
                    Some(std::path::PathBuf::from(format!("/usr/bin/{file}")))
                }
            }
        }
    }
}

/// Walks `$PATH` for an executable named `file`.
fn look_path(file: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    if file.contains('/') {
        let p = std::path::PathBuf::from(file);
        return p.is_file().then_some(p);
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let candidate = dir.join(file);
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            return Some(candidate);
        }
    }
    None
}

async fn exec_run<F>(c: &Cmd, mut on_line: Option<F>) -> (Output, Result<(), RunError>)
where
    F: FnMut(&str),
{
    let Some((program, rest)) = c.args.split_first() else {
        return (
            Output {
                exit_code: -1,
                ..Default::default()
            },
            Err(RunError::Spawn {
                cmd: String::new(),
                source: std::io::Error::other("empty command"),
            }),
        );
    };

    // stderr has to be piped whenever a line callback is watching, even if the
    // caller asked for Inherit — the lines are re-emitted below, so nothing is
    // lost by intercepting them here.
    let stderr_piped = on_line.is_some() || c.stderr == Sink::Capture;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest)
        .stdin(if c.stdin_inherit {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(c.stdout.to_stdio())
        .stderr(if stderr_piped {
            Stdio::piped()
        } else {
            c.stderr.to_stdio()
        });

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return (
                Output {
                    exit_code: -1,
                    ..Default::default()
                },
                Err(RunError::Spawn {
                    cmd: c.to_string(),
                    source: e,
                }),
            );
        }
    };

    let mut out = Output::default();

    // Both pipes are drained concurrently, and this is not a nicety: a pipe
    // holds about 64 KiB, and `nix path-info --json` writes megabytes. Reading
    // stderr to EOF first would block the child on a full stdout pipe, and the
    // child would then never close stderr — a deadlock that only shows up on
    // real closures, never on a small test command.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let read_stdout = async {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = tokio::io::AsyncReadExt::read_to_end(p, &mut buf).await;
        }
        buf
    };

    // stderr is read line by line as it arrives, so a callback sees progress
    // rather than a transcript after the fact.
    let read_stderr = async {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let mut lines = BufReader::new(p).lines();
            let mut stderr_out = tokio::io::stderr();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim_end_matches('\r');
                if let Some(f) = on_line.as_mut() {
                    f(line);
                }
                match c.stderr {
                    Sink::Capture => {
                        buf.extend_from_slice(line.as_bytes());
                        buf.push(b'\n');
                    }
                    Sink::Inherit => {
                        let _ = stderr_out.write_all(line.as_bytes()).await;
                        let _ = stderr_out.write_all(b"\n").await;
                    }
                    Sink::Null => {}
                }
            }
        }
        buf
    };

    let (stdout, stderr) = tokio::join!(read_stdout, read_stderr);
    out.stdout = stdout;
    out.stderr = stderr;

    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => {
            out.exit_code = -1;
            return (
                out,
                Err(RunError::Spawn {
                    cmd: c.to_string(),
                    source: e,
                }),
            );
        }
    };

    out.exit_code = status.code().unwrap_or(-1);
    if status.success() {
        (out, Ok(()))
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let code = out.exit_code;
        (
            out,
            Err(RunError::Exit {
                cmd: c.to_string(),
                code,
                stderr,
            }),
        )
    }
}

// ---------------------------------------------------------------------------
// The test runner
// ---------------------------------------------------------------------------

type Reply = Box<dyn Fn(&Cmd) -> (Output, Result<(), RunError>) + Send + Sync>;

struct Handler {
    pattern: String,
    reply: Reply,
}

/// A [`Runner`] for tests.
///
/// An unmatched command is a hard error rather than a silent empty success: a
/// test that starts shelling out to something nobody expected should fail
/// loudly, not pass with a zero value.
#[derive(Default)]
pub struct Fake {
    handlers: Mutex<Vec<Handler>>,
    calls: Mutex<Vec<String>>,
    missing: Mutex<Vec<String>>,
    /// Counts per pattern, for tests that assert a command ran exactly once.
    counters: Mutex<HashMap<String, usize>>,
}

impl Fake {
    pub fn new() -> Fake {
        Fake::default()
    }

    /// Replies to commands whose argv, space-joined, either equals `pattern` or
    /// begins with `pattern` followed by a space — so `"nix build"` matches any
    /// build invocation, and a full command line matches only itself.
    pub fn on(&self, pattern: &str, stdout: &str) -> &Fake {
        let stdout = stdout.to_string();
        self.on_func(pattern, move |_| {
            (
                Output {
                    stdout: stdout.clone().into_bytes(),
                    ..Default::default()
                },
                Ok(()),
            )
        })
    }

    /// Replies with a non-zero exit, as a real failing process would.
    pub fn fail(&self, pattern: &str, code: i32, stderr: &str) -> &Fake {
        let stderr = stderr.to_string();
        self.on_func(pattern, move |c| {
            (
                Output {
                    stderr: stderr.clone().into_bytes(),
                    exit_code: code,
                    ..Default::default()
                },
                Err(RunError::Exit {
                    cmd: c.to_string(),
                    code,
                    stderr: stderr.clone(),
                }),
            )
        })
    }

    pub fn on_func<F>(&self, pattern: &str, reply: F) -> &Fake
    where
        F: Fn(&Cmd) -> (Output, Result<(), RunError>) + Send + Sync + 'static,
    {
        self.handlers.lock().unwrap().push(Handler {
            pattern: pattern.to_string(),
            reply: Box::new(reply),
        });
        self
    }

    /// Declares a program absent, so `look_path` reports it missing. Everything
    /// else resolves to `/usr/bin/<name>`.
    pub fn missing(&self, file: &str) -> &Fake {
        self.missing.lock().unwrap().push(file.to_string());
        self
    }

    fn run<F>(&self, c: &Cmd, mut on_line: Option<F>) -> (Output, Result<(), RunError>)
    where
        F: FnMut(&str),
    {
        let line = c.to_string();
        self.calls.lock().unwrap().push(line.clone());
        *self
            .counters
            .lock()
            .unwrap()
            .entry(line.clone())
            .or_insert(0) += 1;

        let handlers = self.handlers.lock().unwrap();
        // Last registration wins, so a test can override a shared setup helper.
        let found = handlers
            .iter()
            .rev()
            .find(|h| matches(&line, &h.pattern))
            .map(|h| (h.reply)(c));
        drop(handlers);

        let Some((out, res)) = found else {
            return (
                Output {
                    exit_code: -1,
                    ..Default::default()
                },
                Err(RunError::Spawn {
                    cmd: line.clone(),
                    source: std::io::Error::other(format!("fake: no handler for {line:?}")),
                }),
            );
        };

        // Reproduce the real runner's plumbing, so callers that rely on
        // streaming are exercised by the fake too rather than only in
        // production — including the rule that a caller who did not ask for
        // capture gets nothing back.
        if let Some(f) = on_line.as_mut() {
            for l in String::from_utf8_lossy(&out.stderr).lines() {
                f(l);
            }
        }
        let out = Output {
            stdout: if c.stdout == Sink::Capture {
                out.stdout
            } else {
                Vec::new()
            },
            stderr: if c.stderr == Sink::Capture {
                out.stderr
            } else {
                Vec::new()
            },
            exit_code: out.exit_code,
        };
        (out, res)
    }

    /// Every command `run` received, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Whether any recorded call matches `pattern`, with the same
    /// equal-or-prefix rule as [`Fake::on`].
    pub fn ran(&self, pattern: &str) -> bool {
        self.count_calls(pattern) > 0
    }

    /// How many recorded calls match `pattern`. Tests that assert a check ran
    /// exactly once — or that a second one was suppressed — need the count, not
    /// just the fact.
    pub fn count_calls(&self, pattern: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches(c, pattern))
            .count()
    }
}

fn matches(line: &str, pattern: &str) -> bool {
    line == pattern || line.starts_with(&format!("{pattern} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Cmd {
        Cmd::new(s.split(' '))
    }

    #[tokio::test]
    async fn captures_stdout_and_reports_success() {
        let r = Runner::Exec;
        let (out, res) = r.run(&cmd("echo hello")).await;
        assert!(res.is_ok());
        assert_eq!(out.out(), "hello");
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_an_error_with_the_output_alongside() {
        let r = Runner::Exec;
        let c = Cmd::new(["sh", "-c", "echo out; echo bad >&2; exit 3"]);
        let (out, res) = r.run(&c).await;
        assert_eq!(out.exit_code, 3);
        assert_eq!(out.out(), "out");
        let err = res.unwrap_err();
        assert_eq!(err.exit_code(), Some(3));
        // The message quotes only the last stderr line: a failing nix build
        // writes hundreds, and the last one is the diagnosis.
        assert!(err.to_string().ends_with(": exit 3: bad"), "{err}");
    }

    #[tokio::test]
    async fn only_the_last_stderr_line_reaches_the_message() {
        let r = Runner::Exec;
        let c = Cmd::new(["sh", "-c", "echo first >&2; echo last >&2; exit 1"]);
        let (_, res) = r.run(&c).await;
        assert!(res.unwrap_err().to_string().ends_with(": exit 1: last"));
    }

    #[tokio::test]
    async fn the_diagnosis_beats_the_last_line_when_they_differ() {
        // nix follows a failed fetch with the server's entire response, so the
        // literally-last line of stderr is as likely to be `</html>` as it is
        // to be the reason the command failed.
        let r = Runner::Exec;
        let c = Cmd::new([
            "sh",
            "-c",
            "echo 'error: unable to download' >&2; echo '</html>' >&2; exit 1",
        ]);
        let (_, res) = r.run(&c).await;
        assert!(
            res.unwrap_err()
                .to_string()
                .ends_with(": exit 1: error: unable to download")
        );
    }

    #[tokio::test]
    async fn stderr_lines_are_delivered_whole_and_stripped_of_cr() {
        let r = Runner::Exec;
        let c = Cmd::new(["sh", "-c", "printf 'one\\r\\ntwo\\n' >&2"]);
        let mut seen = Vec::new();
        let _ = r
            .run_streaming(&c, Some(|l: &str| seen.push(l.to_string())))
            .await;
        assert_eq!(seen, vec!["one", "two"]);
    }

    #[tokio::test]
    async fn a_large_stdout_alongside_stderr_does_not_deadlock() {
        // A pipe holds about 64 KiB. `nix path-info --json` writes megabytes,
        // so draining stderr to EOF first blocks the child on a full stdout
        // pipe — and it then never closes stderr. The deadlock only appears on
        // real closures, so it needs a test that actually exceeds the buffer.
        let r = Runner::Exec;
        let c = Cmd::new([
            "sh",
            "-c",
            "echo starting >&2; for i in $(seq 1 40000); do echo 0123456789012345678901234567890123456789; done; echo done >&2",
        ]);
        let mut seen = Vec::new();
        let (out, res) = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            r.run_streaming(&c, Some(|l: &str| seen.push(l.to_string()))),
        )
        .await
        .expect("deadlocked reading a large stdout");

        assert!(res.is_ok());
        assert!(out.stdout.len() > 1_600_000, "{}", out.stdout.len());
        assert_eq!(seen, vec!["starting", "done"]);
    }

    #[tokio::test]
    async fn a_missing_program_never_ran() {
        let r = Runner::Exec;
        let (_, res) = r.run(&cmd("snout-does-not-exist")).await;
        assert_eq!(res.unwrap_err().exit_code(), None);
    }

    #[tokio::test]
    async fn an_empty_command_is_rejected() {
        let r = Runner::Exec;
        let (_, res) = r.run(&Cmd::new(Vec::<String>::new())).await;
        assert!(res.is_err());
    }

    #[test]
    fn look_path_finds_what_is_there_and_not_what_is_not() {
        assert!(look_path("sh").is_some());
        assert!(look_path("snout-does-not-exist").is_none());
    }

    // -- the fake ----------------------------------------------------------

    #[tokio::test]
    async fn an_unmatched_command_is_a_hard_error() {
        let r = Runner::fake();
        let (_, res) = r.run(&cmd("nix build")).await;
        let err = res.unwrap_err();
        assert!(err.to_string().contains("no handler"), "{err}");
    }

    #[tokio::test]
    async fn matching_is_equal_or_whole_word_prefix() {
        let r = Runner::fake();
        r.as_fake().on("nix build", "built");

        let (out, _) = r.run(&cmd("nix build /flake#x")).await;
        assert_eq!(out.out(), "built");

        // "nix buildx" must not match "nix build": a prefix that stops
        // mid-token would make handlers catch commands nobody meant them to.
        let (_, res) = r.run(&cmd("nix buildx")).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn last_registration_wins() {
        let r = Runner::fake();
        r.as_fake().on("git status", "first");
        r.as_fake().on("git status", "second");
        let (out, _) = r.run(&cmd("git status")).await;
        assert_eq!(out.out(), "second");
    }

    #[tokio::test]
    async fn the_fake_withholds_output_the_caller_did_not_ask_to_capture() {
        // The real runner returns nothing in `stdout` when the stream was
        // inherited. A fake that returned it anyway would let a test pass on a
        // code path that reads empty bytes in production.
        let r = Runner::fake();
        r.as_fake().on("nix flake update", "noise");
        let mut c = cmd("nix flake update");
        c.stdout = Sink::Inherit;
        let (out, _) = r.run(&c).await;
        assert!(out.stdout.is_empty());
    }

    #[tokio::test]
    async fn the_fake_streams_stderr_lines_like_the_real_one() {
        let r = Runner::fake();
        r.as_fake().on_func("nix build", |_| {
            (
                Output {
                    stderr: b"copying\nbuilding\n".to_vec(),
                    ..Default::default()
                },
                Ok(()),
            )
        });
        let mut seen = Vec::new();
        let (_, _) = r
            .run_streaming(&cmd("nix build"), Some(|l: &str| seen.push(l.to_string())))
            .await;
        assert_eq!(seen, vec!["copying", "building"]);
    }

    #[tokio::test]
    async fn fail_reports_the_exit_status() {
        let r = Runner::fake();
        r.as_fake().fail("git commit", 1, "nothing to commit");
        let (out, res) = r.run(&cmd("git commit -q")).await;
        assert_eq!(out.exit_code, 1);
        assert_eq!(res.unwrap_err().exit_code(), Some(1));
    }

    #[tokio::test]
    async fn calls_ran_and_count_are_recorded() {
        let r = Runner::fake();
        r.as_fake().on("nix", "");
        let _ = r.run(&cmd("nix build a")).await;
        let _ = r.run(&cmd("nix build b")).await;
        let _ = r.run(&cmd("nix flake update")).await;
        assert_eq!(r.as_fake().count_calls("nix build"), 2);
        assert!(r.as_fake().ran("nix flake update"));
        assert!(!r.as_fake().ran("sudo"));
        assert_eq!(r.as_fake().calls().len(), 3);
    }

    #[test]
    fn look_path_honours_missing() {
        let r = Runner::fake();
        r.as_fake().missing("git");
        assert!(r.look_path("git").is_none());
        assert_eq!(
            r.look_path("pkill").unwrap(),
            std::path::PathBuf::from("/usr/bin/pkill")
        );
    }
}
