// SPDX-License-Identifier: GPL-2.0-only

//! The signals that ask the daemon to stop (interrupt, terminate, hang-up),
//! received by one thread instead of killing the process outright, so the
//! adapters can be taken off the bus first.

use std::io;

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::c_int;

    /// `sigset_t` in <sys/_types.h>: one bit per signal, signal 1 in bit 0.
    pub type SigSet = u32;

    pub const SIGHUP: c_int = 1;
    pub const SIGINT: c_int = 2;
    pub const SIGTERM: c_int = 15;
    pub const SIG_BLOCK: c_int = 1;
    pub const SIG_UNBLOCK: c_int = 2;

    pub const STOP_SIGNALS: SigSet = 1 << (SIGHUP - 1) | 1 << (SIGINT - 1) | 1 << (SIGTERM - 1);

    extern "C" {
        pub fn pthread_sigmask(how: c_int, set: *const SigSet, old: *mut SigSet) -> c_int;
        pub fn sigwait(set: *const SigSet, signal: *mut c_int) -> c_int;
        pub fn raise(signal: c_int) -> c_int;
    }
}

/// Hold the stop signals back in the calling thread and in every thread it
/// starts afterwards, leaving them for [`wait`]. Call before starting any
/// thread.
#[cfg(target_os = "macos")]
pub(crate) fn block() -> io::Result<()> {
    // SAFETY: the set is a live local; a null old-set pointer is allowed.
    match unsafe { sys::pthread_sigmask(sys::SIG_BLOCK, &sys::STOP_SIGNALS, std::ptr::null_mut()) }
    {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

/// Wait for a stop signal and return its number. A signal the process was
/// started ignoring (`nohup`) never arrives here.
#[cfg(target_os = "macos")]
pub(crate) fn wait() -> i32 {
    let mut signal = 0;
    loop {
        // SAFETY: both pointers refer to live values of the right types.
        if unsafe { sys::sigwait(&sys::STOP_SIGNALS, &mut signal) } == 0 {
            return signal;
        }
    }
}

/// End the process as `signal` would have ended it had it not been held
/// back, so whoever started the daemon sees the same exit status.
#[cfg(target_os = "macos")]
pub(crate) fn die(signal: i32) -> ! {
    // SAFETY: plain libSystem calls; the set is a live constant.
    unsafe {
        sys::pthread_sigmask(sys::SIG_UNBLOCK, &sys::STOP_SIGNALS, std::ptr::null_mut());
        sys::raise(signal);
    }
    // Only reached if the signal did not end the process after all.
    std::process::exit(128 + signal)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn block() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stop signals are only handled on macOS",
    ))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn wait() -> i32 {
    unreachable!("block() fails on this platform")
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn die(signal: i32) -> ! {
    std::process::exit(128 + signal)
}
