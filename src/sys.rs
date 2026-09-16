use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn check_size(result: libc::ssize_t) -> io::Result<usize> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

pub fn read(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match check_size(unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) }) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

pub fn write(fd: RawFd, data: &[u8]) -> io::Result<usize> {
    loop {
        match check_size(unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) }) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

pub fn write_all(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match write(fd, data) {
            Ok(written) => data = &data[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_writable(fd)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn wait_writable(fd: RawFd) -> io::Result<()> {
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    }];
    poll(&mut fds, -1).map(drop)
}

pub fn poll(fds: &mut [libc::pollfd], timeout_ms: libc::c_int) -> io::Result<usize> {
    loop {
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        match check(result) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result.map(|count| count as usize),
        }
    }
}

pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = check(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    check(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }).map(drop)
}

pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    check(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }).map(drop)
}

pub fn is_terminal(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

pub fn open_pty(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    let size = winsize(rows, cols);
    check(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    })?;
    let fds = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_cloexec(master)?;
    set_cloexec(slave)?;
    Ok(fds)
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

pub fn window_size(fd: RawFd) -> Option<(u16, u16)> {
    let mut size = winsize(0, 0);
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_row > 0 && size.ws_col > 0).then_some((size.ws_row, size.ws_col))
}

pub fn set_window_size(fd: RawFd, rows: u16, cols: u16) -> io::Result<()> {
    let size = winsize(rows, cols);
    check(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) }).map(drop)
}

pub fn signal_foreground(master: RawFd, signal: libc::c_int) {
    let group = unsafe { libc::tcgetpgrp(master) };
    if group > 0 {
        unsafe { libc::kill(-group, signal) };
    }
}

pub struct RawMode {
    fd: RawFd,
    original: libc::termios,
}

impl RawMode {
    pub fn enable(fd: RawFd) -> io::Result<RawMode> {
        let mut original = MaybeUninit::<libc::termios>::uninit();
        check(unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) })?;
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        check(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) })?;
        Ok(RawMode { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

static SIGNAL_WRITER: AtomicI32 = AtomicI32::new(-1);

extern "C" fn record_signal(signal: libc::c_int) {
    let saved_errno = unsafe { *libc::__errno_location() };
    let fd = SIGNAL_WRITER.load(Ordering::Relaxed);
    let byte = signal as u8;
    unsafe {
        libc::write(fd, (&raw const byte).cast(), 1);
        *libc::__errno_location() = saved_errno;
    }
}

/// Delivers each listed signal as a byte on the returned descriptor.
pub fn signal_pipe(signals: &[libc::c_int]) -> io::Result<OwnedFd> {
    let (reader, writer) = pipe()?;
    set_nonblocking(reader.as_raw_fd())?;
    set_nonblocking(writer.as_raw_fd())?;
    let previous = SIGNAL_WRITER.swap(writer.as_raw_fd(), Ordering::Relaxed);
    if previous >= 0 {
        unsafe { libc::close(previous) };
    }
    std::mem::forget(writer);
    for &signal in signals {
        set_handler(signal, record_signal as *const () as libc::sighandler_t)?;
    }
    Ok(reader)
}

pub fn drain_signals(reader: RawFd) -> Vec<libc::c_int> {
    let mut buffer = [0u8; 64];
    let mut signals = Vec::new();
    while let Ok(count @ 1..) = read(reader, &mut buffer) {
        signals.extend(buffer[..count].iter().map(|&byte| libc::c_int::from(byte)));
    }
    signals
}

pub fn ignore_signal(signal: libc::c_int) -> io::Result<()> {
    set_handler(signal, libc::SIG_IGN)
}

pub fn default_signal(signal: libc::c_int) -> io::Result<()> {
    set_handler(signal, libc::SIG_DFL)
}

fn set_handler(signal: libc::c_int, handler: libc::sighandler_t) -> io::Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        check(libc::sigaction(signal, &action, std::ptr::null_mut())).map(drop)
    }
}

pub enum Fork {
    Parent(libc::pid_t),
    Child,
}

pub fn fork() -> io::Result<Fork> {
    match check(unsafe { libc::fork() })? {
        0 => Ok(Fork::Child),
        pid => Ok(Fork::Parent(pid)),
    }
}

pub fn exit_now(code: libc::c_int) -> ! {
    unsafe { libc::_exit(code) }
}

pub fn setsid() -> io::Result<()> {
    check(unsafe { libc::setsid() }).map(drop)
}

pub fn make_controlling_terminal(fd: RawFd) -> io::Result<()> {
    check(unsafe { libc::ioctl(fd, libc::TIOCSCTTY, 0) }).map(drop)
}

pub fn dup2(from: RawFd, to: RawFd) -> io::Result<()> {
    check(unsafe { libc::dup2(from, to) }).map(drop)
}

pub fn redirect_to_null(fds: &[RawFd]) -> io::Result<()> {
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    for &fd in fds {
        dup2(null.as_raw_fd(), fd)?;
    }
    Ok(())
}

/// Replaces the process image, returning only on failure.
pub fn exec(program: &CString, args: &[CString]) -> io::Error {
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
    argv.push(std::ptr::null());
    unsafe { libc::execvp(program.as_ptr(), argv.as_ptr()) };
    io::Error::last_os_error()
}

pub enum Status {
    Running,
    Exited(i32),
}

pub fn try_wait(pid: libc::pid_t) -> io::Result<Status> {
    let mut status = 0;
    let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    match check(result)? {
        0 => Ok(Status::Running),
        _ if libc::WIFEXITED(status) => Ok(Status::Exited(libc::WEXITSTATUS(status))),
        _ if libc::WIFSIGNALED(status) => Ok(Status::Exited(128 + libc::WTERMSIG(status))),
        _ => Ok(Status::Running),
    }
}

pub fn wait(pid: libc::pid_t) -> io::Result<()> {
    let mut status = 0;
    loop {
        match check(unsafe { libc::waitpid(pid, &mut status, 0) }) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result.map(drop),
        }
    }
}

pub fn user_id() -> libc::uid_t {
    unsafe { libc::getuid() }
}

pub fn effective_user_id() -> libc::uid_t {
    unsafe { libc::geteuid() }
}
