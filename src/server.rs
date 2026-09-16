use std::ffi::{CString, OsString};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::modes::ModeTracker;
use crate::protocol::{self, Decoder, Message};
use crate::sessions;
use crate::sys::{self, Fork, Status};

const CLIENT_BACKLOG_LIMIT: usize = 1024 * 1024;
const PTY_BACKLOG_LIMIT: usize = 64 * 1024;
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const FIRST_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const EXITED_POLL_MS: libc::c_int = 50;

pub struct Launch {
    pub name: String,
    pub command: Vec<OsString>,
    pub rows: u16,
    pub cols: u16,
}

/// Starts a session daemon and returns once its command is running.
pub fn launch(launch: Launch) -> Result<(), String> {
    let path = sessions::socket_path(&launch.name).map_err(|error| error.to_string())?;
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            return Err(format!("session {} already exists", launch.name));
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let program = command_line(&launch.command)?;
    let (status_reader, status_writer) = sys::pipe().map_err(|error| error.to_string())?;

    match sys::fork().map_err(|error| error.to_string())? {
        Fork::Parent(pid) => {
            drop(status_writer);
            drop(listener);
            sys::wait(pid).map_err(|error| error.to_string())?;
            let mut report = String::new();
            std::fs::File::from(status_reader)
                .read_to_string(&mut report)
                .map_err(|error| error.to_string())?;
            if report.is_empty() {
                Ok(())
            } else {
                Err(report)
            }
        }
        Fork::Child => {
            drop(status_reader);
            let daemonized = sys::setsid().and_then(|()| sys::fork());
            match daemonized {
                Ok(Fork::Parent(_)) => sys::exit_now(0),
                Ok(Fork::Child) => {}
                Err(error) => {
                    report_and_exit(&status_writer, &path, &error.to_string());
                }
            }
            match start(&launch, &program, listener, &path) {
                Ok(server) => {
                    drop(status_writer);
                    let code = match server.run() {
                        Ok(()) => 0,
                        Err(_) => 1,
                    };
                    let _ = std::fs::remove_file(&path);
                    sys::exit_now(code)
                }
                Err(message) => report_and_exit(&status_writer, &path, &message),
            }
        }
    }
}

fn report_and_exit(status_writer: &OwnedFd, path: &Path, message: &str) -> ! {
    let _ = std::fs::remove_file(path);
    let _ = sys::write_all(status_writer.as_raw_fd(), message.as_bytes());
    sys::exit_now(1)
}

fn command_line(command: &[OsString]) -> Result<Vec<CString>, String> {
    let command = if command.is_empty() {
        vec![
            std::env::var_os("SHELL")
                .filter(|shell| !shell.is_empty())
                .unwrap_or_else(|| "/bin/sh".into()),
        ]
    } else {
        command.to_vec()
    };
    command
        .into_iter()
        .map(|arg| CString::new(arg.into_vec()).map_err(|_| "command contains a NUL byte".into()))
        .collect()
}

fn start(
    launch: &Launch,
    program: &[CString],
    listener: UnixListener,
    path: &Path,
) -> Result<Server, String> {
    let fail = |error: io::Error| error.to_string();
    sys::redirect_to_null(&[0, 1, 2]).map_err(fail)?;
    sys::ignore_signal(libc::SIGPIPE).map_err(fail)?;
    sys::ignore_signal(libc::SIGHUP).map_err(fail)?;
    let signals = sys::signal_pipe(&[libc::SIGCHLD, libc::SIGTERM]).map_err(fail)?;
    listener.set_nonblocking(true).map_err(fail)?;
    let (master, slave) = sys::open_pty(launch.rows, launch.cols).map_err(fail)?;
    // SAFETY: the daemon is single-threaded.
    unsafe { std::env::set_var("BOOP_SESSION", &launch.name) };
    let child = spawn(program, &slave)?;
    drop(slave);
    sys::set_nonblocking(master.as_raw_fd()).map_err(fail)?;
    Ok(Server {
        listener: Some(listener),
        path: path.to_path_buf(),
        signals,
        master,
        master_open: true,
        child,
        exit_code: None,
        tracker: ModeTracker::default(),
        clients: Vec::new(),
        pty_backlog: Vec::new(),
        early_output: Some(Vec::new()),
    })
}

fn spawn(program: &[CString], slave: &OwnedFd) -> Result<libc::pid_t, String> {
    let (error_reader, error_writer) = sys::pipe().map_err(|error| error.to_string())?;
    match sys::fork().map_err(|error| error.to_string())? {
        Fork::Child => {
            let error = prepare_child(slave.as_raw_fd())
                .err()
                .unwrap_or_else(|| sys::exec(&program[0], program));
            let message = format!("cannot run {}: {error}", program[0].to_string_lossy());
            let _ = sys::write_all(error_writer.as_raw_fd(), message.as_bytes());
            sys::exit_now(127)
        }
        Fork::Parent(pid) => {
            drop(error_writer);
            let mut message = String::new();
            let _ = std::fs::File::from(error_reader).read_to_string(&mut message);
            if message.is_empty() {
                Ok(pid)
            } else {
                let _ = sys::wait(pid);
                Err(message)
            }
        }
    }
}

fn prepare_child(slave: RawFd) -> io::Result<()> {
    for signal in [libc::SIGPIPE, libc::SIGHUP, libc::SIGCHLD, libc::SIGTERM] {
        sys::default_signal(signal)?;
    }
    sys::setsid()?;
    sys::make_controlling_terminal(slave)?;
    for fd in 0..3 {
        sys::dup2(slave, fd)?;
    }
    Ok(())
}

struct Client {
    stream: UnixStream,
    decoder: Decoder,
    outbox: Vec<u8>,
    attached: bool,
    closing: bool,
    dropped: bool,
}

impl Client {
    fn send(&mut self, message: &Message) {
        self.outbox.extend(message.encode());
        if self.outbox.len() > CLIENT_BACKLOG_LIMIT {
            self.dropped = true;
        }
    }

    fn close_with(&mut self, message: &Message) {
        self.send(message);
        self.closing = true;
    }

    fn flush(&mut self) {
        while !self.outbox.is_empty() {
            match sys::write(self.stream.as_raw_fd(), &self.outbox) {
                Ok(written) => {
                    self.outbox.drain(..written);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.dropped = true;
                    break;
                }
            }
        }
        if self.closing && self.outbox.is_empty() {
            self.dropped = true;
        }
    }
}

struct Server {
    listener: Option<UnixListener>,
    path: PathBuf,
    signals: OwnedFd,
    master: OwnedFd,
    master_open: bool,
    child: libc::pid_t,
    exit_code: Option<i32>,
    tracker: ModeTracker,
    clients: Vec<Client>,
    pty_backlog: Vec<u8>,
    early_output: Option<Vec<u8>>,
}

fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

impl Server {
    fn run(mut self) -> io::Result<()> {
        let first_client_deadline = Instant::now() + FIRST_CLIENT_TIMEOUT;
        loop {
            let awaiting_first_client =
                self.early_output.is_some() && Instant::now() < first_client_deadline;
            match self.exit_code {
                None => self.step(-1)?,
                Some(_) if awaiting_first_client => self.step(EXITED_POLL_MS)?,
                Some(_) => return self.finish(),
            }
        }
    }

    fn step(&mut self, timeout_ms: libc::c_int) -> io::Result<()> {
        let listener = self.listener.as_ref().map_or(-1, AsRawFd::as_raw_fd);
        let mut master_events = 0;
        if self.master_open {
            master_events |= libc::POLLIN;
            if !self.pty_backlog.is_empty() {
                master_events |= libc::POLLOUT;
            }
        }
        let accepting_input = self.pty_backlog.len() < PTY_BACKLOG_LIMIT;
        let mut fds = vec![
            pollfd(listener, libc::POLLIN),
            pollfd(self.signals.as_raw_fd(), libc::POLLIN),
            pollfd(
                if master_events == 0 {
                    -1
                } else {
                    self.master.as_raw_fd()
                },
                master_events,
            ),
        ];
        for client in &self.clients {
            let mut events = 0;
            if accepting_input && !client.closing {
                events |= libc::POLLIN;
            }
            if !client.outbox.is_empty() {
                events |= libc::POLLOUT;
            }
            fds.push(pollfd(client.stream.as_raw_fd(), events));
        }
        sys::poll(&mut fds, timeout_ms)?;

        if fds[1].revents != 0 {
            self.handle_signals();
        }
        if fds[2].revents & libc::POLLOUT != 0 {
            self.write_pty();
        }
        if fds[2].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            self.read_pty();
        }
        if fds[2].revents & libc::POLLNVAL != 0 {
            self.master_open = false;
        }
        for (index, fd) in fds[3..].iter().enumerate() {
            if fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                self.read_client(index);
            }
            if fd.revents & libc::POLLOUT != 0 {
                self.clients[index].flush();
            }
        }
        self.clients.retain(|client| !client.dropped);
        if fds[0].revents & libc::POLLIN != 0 {
            self.accept();
        }
        Ok(())
    }

    fn handle_signals(&mut self) {
        for signal in sys::drain_signals(self.signals.as_raw_fd()) {
            if signal == libc::SIGTERM {
                sys::signal_foreground(self.master.as_raw_fd(), libc::SIGHUP);
                unsafe { libc::kill(self.child, libc::SIGHUP) };
            }
        }
        if let Ok(Status::Exited(code)) = sys::try_wait(self.child) {
            self.exit_code = Some(code);
        }
    }

    fn accept(&mut self) {
        let Some(listener) = &self.listener else {
            return;
        };
        while let Ok((stream, _)) = listener.accept() {
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            self.clients.push(Client {
                stream,
                decoder: Decoder::default(),
                outbox: Vec::new(),
                attached: false,
                closing: false,
                dropped: false,
            });
        }
    }

    /// Reads one chunk from the pty, returning whether data arrived.
    fn read_pty(&mut self) -> bool {
        let mut buffer = [0u8; protocol::MAX_PAYLOAD];
        match sys::read(self.master.as_raw_fd(), &mut buffer) {
            Ok(0) => self.master_open = false,
            Ok(count) => {
                self.broadcast(&buffer[..count]);
                return true;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => self.master_open = false,
        }
        false
    }

    fn broadcast(&mut self, data: &[u8]) {
        self.tracker.scan(data);
        if let Some(early) = &mut self.early_output {
            if early.len() + data.len() > CLIENT_BACKLOG_LIMIT {
                *early = self.tracker.replay();
            } else {
                early.extend_from_slice(data);
            }
        }
        let message = Message::Output(data.to_vec());
        for client in self.clients.iter_mut().filter(|client| client.attached) {
            client.send(&message);
        }
    }

    fn write_pty(&mut self) {
        match sys::write(self.master.as_raw_fd(), &self.pty_backlog) {
            Ok(written) => {
                self.pty_backlog.drain(..written);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => self.pty_backlog.clear(),
        }
    }

    fn read_client(&mut self, index: usize) {
        let mut buffer = [0u8; 16 * 1024];
        let client = &mut self.clients[index];
        match sys::read(client.stream.as_raw_fd(), &mut buffer) {
            Ok(0) => {
                client.dropped = true;
                return;
            }
            Ok(count) => client.decoder.push(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
            Err(_) => {
                client.dropped = true;
                return;
            }
        }
        loop {
            let client = &mut self.clients[index];
            if client.closing || client.dropped {
                return;
            }
            match client.decoder.next_message() {
                Ok(Some(message)) => self.handle_message(index, message),
                Ok(None) => return,
                Err(_) => {
                    client.dropped = true;
                    return;
                }
            }
        }
    }

    fn handle_message(&mut self, index: usize, message: Message) {
        let attached = self.clients[index].attached;
        match message {
            Message::Attach {
                version,
                rows,
                cols,
            } if !attached => {
                if !self.version_matches(index, version) {
                    return;
                }
                let initial = self
                    .early_output
                    .take()
                    .unwrap_or_else(|| self.tracker.replay());
                let client = &mut self.clients[index];
                client.attached = true;
                for chunk in protocol::chunks(&initial) {
                    client.send(&Message::Output(chunk.to_vec()));
                }
                self.resize(rows, cols);
                sys::signal_foreground(self.master.as_raw_fd(), libc::SIGWINCH);
            }
            Message::Input(data) if attached => self.pty_backlog.extend(data),
            Message::Resize { rows, cols } if attached => self.resize(rows, cols),
            Message::Disconnect { version } if !attached => {
                if !self.version_matches(index, version) {
                    return;
                }
                for (other, client) in self.clients.iter_mut().enumerate() {
                    if other == index || client.attached {
                        client.close_with(&Message::Detached);
                    }
                }
            }
            _ => self.clients[index].dropped = true,
        }
    }

    fn version_matches(&mut self, index: usize, version: u8) -> bool {
        let matches = version == protocol::VERSION;
        if !matches {
            self.clients[index].close_with(&Message::Error(format!(
                "speaks protocol {} but the client speaks {version}; restart the session",
                protocol::VERSION
            )));
        }
        matches
    }

    fn resize(&self, rows: u16, cols: u16) {
        if rows > 0 && cols > 0 {
            let _ = sys::set_window_size(self.master.as_raw_fd(), rows, cols);
        }
    }

    fn finish(mut self) -> io::Result<()> {
        let _ = std::fs::remove_file(&self.path);
        self.listener = None;
        let mut drained = 0;
        while self.master_open && drained < 64 && self.read_pty() {
            drained += 1;
        }
        let code = self.exit_code.unwrap_or(0);
        for client in self.clients.iter_mut() {
            if client.attached {
                client.close_with(&Message::Exit(code));
            } else {
                client.dropped = true;
            }
        }
        let deadline = Instant::now() + EXIT_FLUSH_TIMEOUT;
        loop {
            self.clients.retain(|client| !client.dropped);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if self.clients.is_empty() || remaining.is_zero() {
                return Ok(());
            }
            let mut fds: Vec<_> = self
                .clients
                .iter()
                .map(|client| pollfd(client.stream.as_raw_fd(), libc::POLLOUT))
                .collect();
            sys::poll(&mut fds, remaining.as_millis() as libc::c_int)?;
            for (client, fd) in self.clients.iter_mut().zip(&fds) {
                if fd.revents != 0 {
                    client.flush();
                }
            }
        }
    }
}
