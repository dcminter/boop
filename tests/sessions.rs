use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);
const DETACH: &[u8] = b"\x1c";

struct Env {
    runtime: PathBuf,
    work: PathBuf,
}

impl Env {
    fn new(test: &str) -> Env {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
        let _ = std::fs::remove_dir_all(&base);
        let runtime = base.join("run");
        let work = base.join("work");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Env { runtime, work }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_boop"));
        command
            .args(args)
            .current_dir(&self.work)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("SHELL", "/bin/sh")
            .env("PS1", "$ ")
            .env("TERM", "xterm")
            .env_remove("BOOP_SESSION")
            .env_remove("BOOP_DETACH_KEY")
            .env_remove("ENV");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    fn list(&self) -> Vec<String> {
        let output = self.run(&["--list"]);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn terminal(&self, args: &[&str]) -> Terminal {
        self.terminal_with(args, 24, 80, |_| {})
    }

    fn terminal_with(
        &self,
        args: &[&str],
        rows: u16,
        cols: u16,
        configure: impl FnOnce(&mut Command),
    ) -> Terminal {
        let mut command = self.command(args);
        configure(&mut command);
        Terminal::spawn(command, rows, cols)
    }

    fn socket_dir(&self) -> PathBuf {
        self.runtime.join("boop")
    }

    fn wait_for_file(&self, name: &str) {
        let path = self.work.join(name);
        let deadline = Instant::now() + TIMEOUT;
        while !path.exists() {
            assert!(Instant::now() < deadline, "{name} never appeared");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let marker = format!("XDG_RUNTIME_DIR={}", self.runtime.display()).into_bytes();
        let Ok(processes) = std::fs::read_dir("/proc") else {
            return;
        };
        for process in processes.flatten() {
            let Some(pid) = process.file_name().to_str().and_then(|n| n.parse().ok()) else {
                continue;
            };
            let Ok(environment) = std::fs::read(process.path().join("environ")) else {
                continue;
            };
            if environment
                .split(|&byte| byte == 0)
                .any(|entry| entry == marker)
            {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

struct Terminal {
    master: File,
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    seen: usize,
    hung_up: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
}

impl Terminal {
    fn spawn(mut command: Command, rows: u16, cols: u16) -> Terminal {
        let (mut master, mut slave) = (0, 0);
        let size = winsize(rows, cols);
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        };
        assert_eq!(result, 0);
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(command);
        drop(slave);

        let output = Arc::new(Mutex::new(Vec::new()));
        let hung_up = Arc::new(AtomicBool::new(false));
        let mut reader = master.try_clone().unwrap();
        let sink = Arc::clone(&output);
        let stop = Arc::clone(&hung_up);
        let reader = thread::spawn(move || {
            let mut buffer = [0u8; 65536];
            while !stop.load(Ordering::Relaxed) {
                let mut fds = [libc::pollfd {
                    fd: reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }];
                if unsafe { libc::poll(fds.as_mut_ptr(), 1, 50) } <= 0 {
                    continue;
                }
                match reader.read(&mut buffer) {
                    Ok(count @ 1..) => sink.lock().unwrap().extend_from_slice(&buffer[..count]),
                    _ => break,
                }
            }
        });
        Terminal {
            master,
            child,
            output,
            seen: 0,
            hung_up,
            reader: Some(reader),
        }
    }

    fn wait_raw(&self) {
        wait_until(
            || {
                let mut termios: libc::termios = unsafe { std::mem::zeroed() };
                let result = unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut termios) };
                result == 0 && termios.c_lflag & libc::ICANON == 0
            },
            "raw mode",
        );
    }

    fn hang_up(&mut self) {
        self.hung_up.store(true, Ordering::Relaxed);
        self.reader.take().unwrap().join().unwrap();
        self.master = File::open("/dev/null").unwrap();
    }

    fn send(&mut self, data: &[u8]) {
        self.master.write_all(data).unwrap();
    }

    fn line(&mut self, line: &str) {
        self.send(format!("{line}\r").as_bytes());
    }

    fn output(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output()).into_owned()
    }

    /// Waits for bytes after the previous match and consumes through them.
    fn expect(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let output = self.output();
            if let Some(position) = find(&output[self.seen..], needle.as_bytes()) {
                self.seen += position + needle.len();
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; got {:?}",
                String::from_utf8_lossy(&output[self.seen..])
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn resize(&self, rows: u16, cols: u16) {
        let size = winsize(rows, cols);
        let result = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
        assert_eq!(result, 0);
    }

    fn wait(&mut self) -> i32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap());
            }
            assert!(
                Instant::now() < deadline,
                "client did not exit; output {:?}",
                self.text()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn detach(&mut self) {
        self.send(DETACH);
        assert_eq!(self.wait(), 0);
    }

    fn wait_disconnected(&mut self, session: &str) {
        assert_eq!(self.wait(), 0);
        self.expect(&format!("[detached from {session} by boop --disconnect]"));
    }

    fn wait_detached(&mut self, session: &str) {
        assert_eq!(self.wait(), 0);
        self.expect(&format!("[detached from {session}]"));
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn command_output_and_exit_status() {
    let env = Env::new("command_output_and_exit_status");
    let mut terminal = env.terminal(&["--session", "t", "sh", "-c", "echo hello; exit 3"]);
    terminal.expect("hello");
    assert_eq!(terminal.wait(), 3);
    assert!(env.list().is_empty());
}

#[test]
fn fast_command_output_is_kept() {
    let env = Env::new("fast_command_output_is_kept");
    for attempt in 0..5 {
        let mut terminal = env.terminal(&["echo", &format!("quick{attempt}")]);
        terminal.expect(&format!("quick{attempt}"));
        assert_eq!(terminal.wait(), 0);
    }
}

#[test]
fn signalled_command_status() {
    let env = Env::new("signalled_command_status");
    let mut terminal = env.terminal(&["sh", "-c", "kill -TERM $$"]);
    assert_eq!(terminal.wait(), 128 + libc::SIGTERM);
}

#[test]
fn detach_and_reattach() {
    let env = Env::new("detach_and_reattach");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo mark-$((40+1))");
    first.expect("mark-41");
    first.detach();
    first.expect("[detached from t]");
    assert_eq!(env.list(), ["t"]);

    let mut second = env.terminal(&["--name", "t"]);
    second.line("echo mark-$((40+2))");
    second.expect("mark-42");
    second.line("exit 5");
    assert_eq!(second.wait(), 5);
    assert!(env.list().is_empty());
}

#[test]
fn session_option_attaches_to_existing() {
    let env = Env::new("session_option_attaches_to_existing");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("marker=kept");
    first.line("echo ready-$((1+1))");
    first.expect("ready-2");
    first.detach();

    let mut second = env.terminal(&["--session", "t"]);
    second.line("echo value-$marker");
    second.expect("value-kept");
    second.line("exit");
    assert_eq!(second.wait(), 0);
}

#[test]
fn bare_invocation() {
    let env = Env::new("bare_invocation");
    let mut first = env.terminal(&[]);
    first.line("echo in-$BOOP_SESSION");
    first.expect("in-boop");
    first.detach();
    assert_eq!(env.list(), ["boop"]);

    let mut second = env.terminal(&[]);
    second.line("echo again-$BOOP_SESSION");
    second.expect("again-boop");
    second.detach();

    let mut other = env.terminal(&["--session", "other", "sh"]);
    other.line("echo other-$((2+2))");
    other.expect("other-4");
    other.detach();

    let mut third = env.terminal(&[]);
    assert_eq!(third.wait(), 1);
    third.expect("boop other");
    third.expect("boop --name NAME");
    assert_eq!(env.list(), ["boop", "other"]);
}

#[test]
fn bare_invocation_attaches_to_single_named_session() {
    let env = Env::new("bare_invocation_attaches_to_single_named_session");
    let mut first = env.terminal(&["--session", "solo", "sh"]);
    first.line("echo up-$((3+3))");
    first.expect("up-6");
    first.detach();

    let mut second = env.terminal(&[]);
    second.line("echo in-$BOOP_SESSION");
    second.expect("in-solo");
    second.line("exit");
    assert_eq!(second.wait(), 0);
}

#[test]
fn window_size_propagates() {
    let env = Env::new("window_size_propagates");
    let mut first = env.terminal_with(&["--session", "t", "sh"], 20, 70, |_| {});
    first.line("stty size");
    first.expect("20 70");
    first.resize(30, 100);
    thread::sleep(Duration::from_millis(200));
    first.line("stty size");
    first.expect("30 100");
    first.detach();

    let mut second = env.terminal_with(&["--name", "t"], 40, 120, |_| {});
    second.line("stty size");
    second.expect("40 120");
    second.line("exit");
    assert_eq!(second.wait(), 0);
}

#[test]
fn reattach_signals_resize_even_when_size_is_unchanged() {
    let env = Env::new("reattach_signals_resize");
    let script = "trap 'echo winch-$((n=n+1))' WINCH; echo trapped; while :; do sleep 0.05; done";
    let mut first = env.terminal(&["--session", "t", "sh", "-c", script]);
    first.expect("trapped");
    first.detach();

    let mut second = env.terminal(&["--name", "t"]);
    second.expect("winch-");
}

#[test]
fn multiple_clients_share_a_session() {
    let env = Env::new("multiple_clients_share_a_session");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo one-$((0+1))");
    first.expect("one-1");

    let mut second = env.terminal(&["--name", "t"]);
    second.line("echo two-$((1+1))");
    second.expect("two-2");
    first.expect("two-2");

    first.line("echo three-$((1+2))");
    second.expect("three-3");

    second.detach();
    first.line("echo four-$((2+2))");
    first.expect("four-4");
    first.line("exit 7");
    assert_eq!(first.wait(), 7);
}

#[test]
fn disconnect_detaches_all_clients() {
    let env = Env::new("disconnect_detaches_all_clients");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo a-$((0+1))");
    first.expect("a-1");
    let mut second = env.terminal(&["--name", "t"]);
    second.line("echo b-$((0+2))");
    second.expect("b-2");

    let output = env.run(&["--disconnect", "t"]);
    assert!(output.status.success(), "{output:?}");
    first.wait_disconnected("t");
    second.wait_disconnected("t");
    assert_eq!(env.list(), ["t"]);

    let mut third = env.terminal(&["--name", "t"]);
    third.line("echo c-$((0+3))");
    third.expect("c-3");
    third.line("exit");
    assert_eq!(third.wait(), 0);
}

#[test]
fn disconnect_without_clients_succeeds() {
    let env = Env::new("disconnect_without_clients_succeeds");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo up-$((0+1))");
    first.expect("up-1");
    first.detach();
    let output = env.run(&["--disconnect", "t"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(env.list(), ["t"]);
}

#[test]
fn missing_sessions_are_errors() {
    let env = Env::new("missing_sessions_are_errors");
    let output = env.run(&["--disconnect", "absent"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no session named absent"));

    let mut terminal = env.terminal(&["--name", "absent"]);
    assert_eq!(terminal.wait(), 1);
    terminal.expect("no session named absent");
}

#[test]
fn exec_failure_is_reported() {
    let env = Env::new("exec_failure_is_reported");
    let mut terminal = env.terminal(&["--session", "t", "/nonexistent/program"]);
    assert_eq!(terminal.wait(), 1);
    terminal.expect("cannot run /nonexistent/program");
    assert!(env.list().is_empty());
    let leftovers: Vec<_> = std::fs::read_dir(env.socket_dir()).unwrap().collect();
    assert!(leftovers.is_empty());
}

#[test]
fn command_for_existing_session_is_refused() {
    let env = Env::new("command_for_existing_session_is_refused");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo up-$((0+1))");
    first.expect("up-1");
    first.detach();

    let mut second = env.terminal(&["--session", "t", "top"]);
    assert_eq!(second.wait(), 1);
    second.expect("session t already exists");

    let mut third = env.terminal(&["sh"]);
    third.line("echo default-$((0+1))");
    third.expect("default-1");
    third.detach();
    let mut fourth = env.terminal(&["sh"]);
    assert_eq!(fourth.wait(), 1);
    fourth.expect("session boop already exists");
}

#[test]
fn nesting_is_refused() {
    let env = Env::new("nesting_is_refused");
    let mut terminal = env.terminal(&["--session", "t", "sh"]);
    let binary = env!("CARGO_BIN_EXE_boop");
    terminal.line(&format!("{binary} --name t; echo status-$?"));
    terminal.expect("already inside session t");
    terminal.expect("status-1");
    terminal.line(&format!("{binary} --list"));
    terminal.expect("\nt");
    terminal.line("exit");
    assert_eq!(terminal.wait(), 0);
}

#[test]
fn nested_session_with_other_name() {
    let env = Env::new("nested_session_with_other_name");
    let mut outer = env.terminal(&["--session", "outer", "sh"]);
    let binary = env!("CARGO_BIN_EXE_boop");
    outer.line(&format!(
        "{binary} --session inner sh -c 'echo in-$BOOP_SESSION'"
    ));
    outer.expect("in-inner");
    outer.line("echo back-$BOOP_SESSION");
    outer.expect("back-outer");
    outer.line("exit");
    assert_eq!(outer.wait(), 0);
}

#[test]
fn lost_client_leaves_session_running() {
    let env = Env::new("lost_client_leaves_session_running");
    for (signal, status) in [(libc::SIGKILL, 128 + libc::SIGKILL), (libc::SIGHUP, 0)] {
        let mut first = env.terminal(&["--session", "t"]);
        first.line("echo alive-$((1+1))");
        first.expect("alive-2");
        unsafe { libc::kill(first.child.id() as libc::pid_t, signal) };
        assert_eq!(first.wait(), status);
        drop(first);
        assert_eq!(env.list(), ["t"]);
    }

    let mut second = env.terminal(&["--name", "t"]);
    second.line("echo still-$((1+2))");
    second.expect("still-3");
    second.line("exit");
    assert_eq!(second.wait(), 0);
}

#[test]
fn closed_terminal_leaves_session_running() {
    let env = Env::new("closed_terminal_leaves_session_running");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo alive-$((1+1))");
    first.expect("alive-2");
    first.hang_up();
    first.wait();
    assert_eq!(env.list(), ["t"]);
}

#[test]
fn terminated_client_detaches() {
    let env = Env::new("terminated_client_detaches");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo alive-$((1+1))");
    first.expect("alive-2");
    unsafe { libc::kill(first.child.id() as libc::pid_t, libc::SIGTERM) };
    first.wait_detached("t");
    assert_eq!(env.list(), ["t"]);
}

#[test]
fn stale_socket_is_removed() {
    let env = Env::new("stale_socket_is_removed");
    assert!(env.list().is_empty());
    let path = env.socket_dir().join("stale");
    drop(UnixListener::bind(&path).unwrap());
    assert!(std::fs::metadata(&path).unwrap().file_type().is_socket());
    assert!(env.list().is_empty());
    assert!(!path.exists());

    drop(UnixListener::bind(&path).unwrap());
    let mut terminal = env.terminal(&["--session", "stale", "sh", "-c", "echo fresh"]);
    terminal.expect("fresh");
    assert_eq!(terminal.wait(), 0);
}

#[test]
fn foreign_files_are_ignored() {
    let env = Env::new("foreign_files_are_ignored");
    assert!(env.list().is_empty());
    std::fs::write(env.socket_dir().join("plain"), "").unwrap();
    assert!(env.list().is_empty());
}

#[test]
fn insecure_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("insecure_directory_is_refused");
    std::fs::create_dir(env.socket_dir()).unwrap();
    std::fs::set_permissions(env.socket_dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = env.run(&["--list"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("mode 0700"));
}

#[test]
fn custom_detach_key() {
    let env = Env::new("custom_detach_key");
    let script = "stty raw -echo; echo raw-$((0+1)); cat";
    let mut first = env.terminal(&["--detach-key", "^a", "--session", "t", "sh", "-c", script]);
    first.expect("raw-1");
    first.send(b"x\x1cy\r");
    first.expect("x\x1cy");
    first.send(b"\x01");
    first.wait_detached("t");

    let mut second = env.terminal_with(&["--name", "t"], 24, 80, |command| {
        command.env("BOOP_DETACH_KEY", "^B");
    });
    second.wait_raw();
    second.send(b"p\x1cq\r");
    second.expect("p\x1cq");
    second.send(b"\x02");
    second.wait_detached("t");
}

#[test]
fn detach_key_mid_input_forwards_preceding_bytes() {
    let env = Env::new("detach_key_mid_input");
    let mut first = env.terminal(&["--session", "t", "cat"]);
    first.send(b"before\r");
    first.expect("before\r\n");
    first.send(b"partial\x1cignored");
    first.wait_detached("t");

    let mut second = env.terminal(&["--name", "t"]);
    second.send(b"-end\r");
    second.expect("partial-end");
}

#[test]
fn modes_are_replayed_and_reset() {
    let env = Env::new("modes_are_replayed_and_reset");
    let script = "printf '\\033[?1049h\\033[?1000h\\033[?2004hready\\n'; cat";
    let mut first = env.terminal(&["--session", "t", "sh", "-c", script]);
    first.expect("ready");
    first.detach();
    first.expect("\x1b[?1049l\x1b[?1000l\x1b[?2004l\x1b[0m");

    let mut second = env.terminal(&["--name", "t"]);
    second.expect("\x1b[?1049h\x1b[?1000h\x1b[?2004h");
    second.send(b"\x04");
    assert_eq!(second.wait(), 0);
    second.expect("\x1b[?1049l\x1b[?1000l\x1b[?2004l\x1b[0m");
}

#[test]
fn input_bytes_pass_through() {
    let env = Env::new("input_bytes_pass_through");
    let mut terminal = env.terminal(&[
        "--session",
        "t",
        "sh",
        "-c",
        "stty raw -echo; echo raw-$((0+1)); head -c 9 | od -An -tx1; stty sane",
    ]);
    terminal.expect("raw-1");
    terminal.send(b"\x1b[<64;1;1M");
    terminal.expect("1b 5b 3c 36 34 3b 31 3b 31");
    assert_eq!(terminal.wait(), 0);
}

#[test]
fn large_output_arrives_intact() {
    let env = Env::new("large_output_arrives_intact");
    let mut terminal = env.terminal(&["sh", "-c", "seq 1 200000; echo end-$((1+1))"]);
    terminal.expect("\n199999\r\n200000\r\nend-2");
    assert_eq!(terminal.wait(), 0);
    let text = terminal.text();
    assert!(text.contains("\n100000\r\n100001\r\n"));
}

#[test]
fn detached_session_keeps_running() {
    let env = Env::new("detached_session_keeps_running");
    let script = "echo waiting; read go; echo started; seq 1 500000; touch finished; cat";
    let mut first = env.terminal(&["--session", "t", "sh", "-c", script]);
    first.expect("waiting");
    first.detach();
    let mut starter = env.terminal(&["--name", "t"]);
    starter.send(b"\r");
    starter.expect("started");
    starter.send(DETACH);
    assert_eq!(starter.wait(), 0);
    env.wait_for_file("finished");
    wait_until(|| env.list() == ["t"], "session listing");
}

#[test]
fn non_terminal_input_is_refused() {
    let env = Env::new("non_terminal_input_is_refused");
    let output = env.run(&["--session", "t", "sh"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not a terminal"));
    assert!(env.list().is_empty());
}

#[test]
fn help_version_and_usage_errors() {
    let env = Env::new("help_version_and_usage_errors");
    let help = env.run(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--disconnect [NAME]"));

    let version = env.run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("boop {}\n", env!("CARGO_PKG_VERSION"))
    );

    for args in [
        &["--bogus"][..],
        &["--session"],
        &["--session", "a/b"],
        &["--name", "a", "--session", "b"],
        &["--detach-key", "x"],
    ] {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).starts_with("boop: "), "{args:?}");
        assert!(stderr(&output).contains("Usage:"), "{args:?}");
    }

    let output = env
        .command(&["--list"])
        .env("BOOP_DETACH_KEY", "bad")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("BOOP_DETACH_KEY"));
}

#[test]
fn session_runs_in_working_directory() {
    let env = Env::new("session_runs_in_working_directory");
    let mut terminal = env.terminal(&["sh", "-c", "pwd"]);
    terminal.expect(env.work.to_str().unwrap());
    assert_eq!(terminal.wait(), 0);
}

#[test]
fn session_has_controlling_terminal() {
    let env = Env::new("session_has_controlling_terminal");
    let mut terminal = env.terminal(&["sh", "-c", "tty; echo term-$TERM"]);
    terminal.expect("/dev/pts/");
    terminal.expect("term-xterm");
    assert_eq!(terminal.wait(), 0);
}

#[test]
fn context_messages() {
    let env = Env::new("context_messages");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.expect("[started session t; detach with ^\\]");
    first.line("echo up-$((0+1))");
    first.expect("up-1");
    first.detach();
    first.expect("[detached from t]");

    let mut second = env.terminal(&["--detach-key", "^a", "--name", "t"]);
    second.expect("[attached to session t; detach with ^A]");
    second.line("exit 4");
    assert_eq!(second.wait(), 4);
    second.expect("[session t ended with status 4]");
}

#[test]
fn nested_context_is_reported() {
    let env = Env::new("nested_context_is_reported");
    let mut outer = env.terminal(&["--session", "outer", "sh"]);
    let binary = env!("CARGO_BIN_EXE_boop");
    outer.line(&format!("{binary} --session inner sh -c 'exit 2'"));
    outer.expect("[inside session outer]");
    outer.expect("[started session inner; detach with ^\\]");
    outer.expect("[session inner ended with status 2]");
    outer.line("exit 0");
    assert_eq!(outer.wait(), 0);
}

#[test]
fn disconnect_is_reported() {
    let env = Env::new("disconnect_is_reported");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo up-$((0+1))");
    first.expect("up-1");
    let output = env.run(&["--disconnect", "t"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stderr(&output), "[disconnected terminals from session t]\n");
    assert!(output.stdout.is_empty());
    assert_eq!(first.wait(), 0);
    first.expect("[detached from t by boop --disconnect]");
}

#[test]
fn disconnect_without_name_detaches_current_session() {
    let env = Env::new("disconnect_without_name_detaches_current_session");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo up-$((0+1))");
    first.expect("up-1");
    let mut second = env.terminal(&["--name", "t"]);
    second.line("echo b-$((0+2))");
    second.expect("b-2");
    let mut other = env.terminal(&["--session", "u", "sh"]);
    other.line("echo u-$((0+3))");
    other.expect("u-3");

    let binary = env!("CARGO_BIN_EXE_boop");
    second.line(&format!(
        "{binary} --disconnect > status 2>&1; echo $? >> status"
    ));
    first.wait_disconnected("t");
    second.wait_disconnected("t");
    env.wait_for_file("status");
    wait_until(
        || {
            std::fs::read_to_string(env.work.join("status")).unwrap()
                == "[disconnected terminals from session t]\n0\n"
        },
        "disconnect output",
    );
    assert_eq!(env.list(), ["t", "u"]);

    other.line("echo still-$((3+1))");
    other.expect("still-4");
    other.line("exit");
    assert_eq!(other.wait(), 0);
}

#[test]
fn disconnect_without_name_outside_session_is_refused() {
    let env = Env::new("disconnect_without_name_outside_session_is_refused");
    let output = env.run(&["--disconnect"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not inside a session"));

    let output = env
        .command(&["-d"])
        .env("BOOP_SESSION", "a/b")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("BOOP_SESSION: invalid session name"));

    let output = env
        .command(&["-d"])
        .env("BOOP_SESSION", "gone")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no session named gone"));
}

#[test]
fn lost_connection_names_session() {
    let env = Env::new("lost_connection_names_session");
    let mut first = env.terminal(&["--session", "t", "sh"]);
    first.line("echo pid-$$");
    first.expect("pid-");
    first.expect("pid-");
    let text = first.text();
    let pid: libc::pid_t = text
        .rsplit("pid-")
        .next()
        .unwrap()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap();
    let server = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap()
        .rsplit(") ")
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    unsafe { libc::kill(server, libc::SIGKILL) };
    assert_eq!(first.wait(), 1);
    first.expect("boop: session t: lost connection");
}
