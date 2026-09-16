use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use crate::cli::{self, Action};
use crate::modes::ModeTracker;
use crate::protocol::{self, Decoder, Message};
use crate::server::{self, Launch};
use crate::sessions;
use crate::sys::{self, RawMode};

const STDIN: i32 = 0;
const STDOUT: i32 = 1;
const DEFAULT_SIZE: (u16, u16) = (24, 80);

pub fn run(action: Action, detach_key: u8) -> Result<i32, String> {
    match action {
        Action::Auto => auto(detach_key),
        Action::Open { session, command } => open(&session, command, detach_key),
        Action::Attach { session } => {
            refuse_nesting(&session)?;
            let stream = sessions::connect(&session)
                .map_err(describe)?
                .ok_or_else(|| format!("no session named {session}"))?;
            attach(stream, &session, detach_key)
        }
        Action::Disconnect { session } => disconnect(&session),
        Action::List => {
            for name in sessions::list().map_err(describe)? {
                println!("{name}");
            }
            Ok(0)
        }
        Action::Help => {
            print!("{}", cli::USAGE);
            Ok(0)
        }
        Action::Version => {
            println!("boop {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
    }
}

fn describe(error: io::Error) -> String {
    error.to_string()
}

fn auto(detach_key: u8) -> Result<i32, String> {
    let names = sessions::list().map_err(describe)?;
    match names.as_slice() {
        [] => open(cli::DEFAULT_SESSION, Vec::new(), detach_key),
        [name] => open(name, Vec::new(), detach_key),
        _ => Err(format!(
            "sessions running: {}\nchoose one with: boop --name NAME",
            names.join(" ")
        )),
    }
}

fn open(session: &str, command: Vec<OsString>, detach_key: u8) -> Result<i32, String> {
    refuse_nesting(session)?;
    require_terminal()?;
    if let Some(stream) = sessions::connect(session).map_err(describe)? {
        if !command.is_empty() {
            return Err(format!(
                "session {session} already exists; name a new one with --session"
            ));
        }
        return attach(stream, session, detach_key);
    }
    let (rows, cols) = sys::window_size(STDIN).unwrap_or(DEFAULT_SIZE);
    server::launch(Launch {
        name: session.to_string(),
        command,
        rows,
        cols,
    })?;
    let stream = sessions::connect(session)
        .map_err(describe)?
        .ok_or_else(|| format!("session {session} ended at once"))?;
    attach(stream, session, detach_key)
}

fn refuse_nesting(session: &str) -> Result<(), String> {
    match std::env::var("BOOP_SESSION") {
        Ok(current) if current == session => Err(format!("already inside session {session}")),
        _ => Ok(()),
    }
}

fn require_terminal() -> Result<(), String> {
    if sys::is_terminal(STDIN) {
        Ok(())
    } else {
        Err("standard input is not a terminal".into())
    }
}

fn disconnect(session: &str) -> Result<i32, String> {
    let mut stream = sessions::connect(session)
        .map_err(describe)?
        .ok_or_else(|| format!("no session named {session}"))?;
    let request = Message::Disconnect {
        version: protocol::VERSION,
    };
    stream.write_all(&request.encode()).map_err(describe)?;
    let mut decoder = Decoder::default();
    let mut buffer = [0u8; 4096];
    loop {
        match decoder.next_message()? {
            Some(Message::Detached) => return Ok(0),
            Some(Message::Error(text)) => return Err(text),
            Some(_) => continue,
            None => {}
        }
        match stream.read(&mut buffer).map_err(describe)? {
            0 => return Err(format!("session {session} closed the connection")),
            count => decoder.push(&buffer[..count]),
        }
    }
}

enum Outcome {
    Detached,
    Exited(i32),
}

fn attach(mut stream: UnixStream, session: &str, detach_key: u8) -> Result<i32, String> {
    require_terminal()?;
    let signals =
        sys::signal_pipe(&[libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP]).map_err(describe)?;
    let (rows, cols) = sys::window_size(STDIN).unwrap_or(DEFAULT_SIZE);
    let request = Message::Attach {
        version: protocol::VERSION,
        rows,
        cols,
    };
    stream.write_all(&request.encode()).map_err(describe)?;

    let raw_mode = RawMode::enable(STDIN).map_err(describe)?;
    let mut tracker = ModeTracker::default();
    let result = relay(&mut stream, signals.as_raw_fd(), detach_key, &mut tracker);
    let _ = sys::write_all(STDOUT, &tracker.reset());
    drop(raw_mode);

    match result? {
        Outcome::Detached => {
            let _ = writeln!(io::stderr(), "[detached from {session}]");
            Ok(0)
        }
        Outcome::Exited(code) => Ok(code),
    }
}

fn relay(
    stream: &mut UnixStream,
    signals: i32,
    detach_key: u8,
    tracker: &mut ModeTracker,
) -> Result<Outcome, String> {
    let mut decoder = Decoder::default();
    let mut buffer = [0u8; protocol::MAX_PAYLOAD];
    let socket = stream.as_raw_fd();
    loop {
        let mut fds = [STDIN, socket, signals].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        sys::poll(&mut fds, -1).map_err(describe)?;

        if fds[2].revents != 0 {
            for signal in sys::drain_signals(signals) {
                match signal {
                    libc::SIGWINCH => {
                        if let Some((rows, cols)) = sys::window_size(STDIN) {
                            let resize = Message::Resize { rows, cols };
                            stream.write_all(&resize.encode()).map_err(describe)?;
                        }
                    }
                    _ => return Ok(Outcome::Detached),
                }
            }
        }

        if fds[1].revents != 0 {
            let count = match sys::read(socket, &mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(format!("lost connection to session: {error}")),
            };
            if count == 0 {
                return Err("lost connection to session".into());
            }
            decoder.push(&buffer[..count]);
            while let Some(message) = decoder.next_message()? {
                match message {
                    Message::Output(data) => {
                        tracker.scan(&data);
                        sys::write_all(STDOUT, &data).map_err(describe)?;
                    }
                    Message::Exit(code) => return Ok(Outcome::Exited(code)),
                    Message::Detached => return Ok(Outcome::Detached),
                    Message::Error(text) => return Err(text),
                    _ => return Err("unexpected message from session".into()),
                }
            }
        }

        if fds[0].revents != 0 {
            let count = match sys::read(STDIN, &mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(describe(error)),
            };
            let input = &buffer[..count];
            let detach_at = input.iter().position(|&byte| byte == detach_key);
            let forwarded = &input[..detach_at.unwrap_or(count)];
            for chunk in protocol::chunks(forwarded) {
                let message = Message::Input(chunk.to_vec());
                stream.write_all(&message.encode()).map_err(describe)?;
            }
            if count == 0 || detach_at.is_some() {
                return Ok(Outcome::Detached);
            }
        }
    }
}
