use std::{fs::File, os::fd::AsRawFd};

use crate::error::Result;

/// Takes a shared (read) flock on `file`. Blocks until the lock is free.
pub fn flock_shared(file: &File) -> Result<()> {
    flock(file, libc::LOCK_SH)
}

/// Takes an exclusive (write) flock on `file`. Blocks until the lock is free.
pub fn flock_exclusive(file: &File) -> Result<()> {
    flock(file, libc::LOCK_EX)
}

/// Tries a non-blocking exclusive flock. Returns `Ok(true)` when it takes the lock, `Ok(false)`
/// when another process holds it.
pub fn try_flock_exclusive(file: &File) -> Result<bool> {
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error.into())
    }
}

fn flock(file: &File, operation: libc::c_int) -> Result<()> {
    loop {
        let ret = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if ret == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}
