use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::cli;
use crate::sys;

pub fn directory() -> io::Result<PathBuf> {
    let directory = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) if !runtime.is_empty() => PathBuf::from(runtime).join("boop"),
        _ => PathBuf::from(format!("/tmp/boop-{}", sys::user_id())),
    };
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir()
        || metadata.uid() != sys::effective_user_id()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a directory owned by the user with mode 0700",
                directory.display()
            ),
        ));
    }
    Ok(directory)
}

pub fn socket_path(name: &str) -> io::Result<PathBuf> {
    Ok(directory()?.join(name))
}

/// Connects to a session, removing its socket if nothing is listening.
pub fn connect(name: &str) -> io::Result<Option<UnixStream>> {
    let path = socket_path(name)?;
    match UnixStream::connect(&path) {
        Ok(stream) => Ok(Some(stream)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(None),
            }
        }
        Err(error) => Err(error),
    }
}

pub fn list() -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory()?)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if cli::session_name(name.clone()).is_err() || !entry.file_type()?.is_socket() {
            continue;
        }
        if connect(&name)?.is_some() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}
