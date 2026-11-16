// SPDX-License-Identifier: GPL-2.0-only

//! Running under launchd.
//!
//! As a launch agent the daemon starts at login and launchd owns the
//! listening socket, so the socket exists before the daemon does: a client
//! that connects early waits in the socket's queue instead of failing, and
//! the daemon is started again on demand if it ever exits.

use std::io;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

/// The launchd label of the agent, also the name of its property list.
pub const LABEL: &str = "org.darwincan.darwincand";

/// The name of the socket in the agent's `Sockets` dictionary.
const SOCKET_NAME: &str = "Listeners";

/// Where the agent's property list lives for the current user.
pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Where the agent's standard error (the daemon's log) goes.
pub fn log_path(home: &Path) -> PathBuf {
    home.join("Library/Logs/darwincand.log")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launch agent property list: run `program --launchd extra_args...` at
/// login with launchd listening on `socket` (owner-only) on its behalf.
pub fn agent_plist(program: &Path, extra_args: &[String], socket: &Path, log: &Path) -> String {
    let mut args = format!(
        "    <string>{}</string>\n    <string>--launchd</string>\n",
        xml_escape(&program.to_string_lossy())
    );
    for arg in extra_args {
        args.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>Sockets</key>
  <dict>
    <key>{SOCKET_NAME}</key>
    <dict>
      <key>SockPathName</key>
      <string>{socket}</string>
      <key>SockPathMode</key>
      <integer>384</integer>
    </dict>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        socket = xml_escape(&socket.to_string_lossy()),
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// The socket path recorded in an agent property list written by
/// [`agent_plist`].
pub fn socket_in_plist(plist: &str) -> Option<PathBuf> {
    let after = plist.split("<key>SockPathName</key>").nth(1)?;
    let value = after.split("<string>").nth(1)?.split("</string>").next()?;
    Some(PathBuf::from(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    ))
}

/// The current user's id, for the `gui/<uid>` launchd domain.
pub fn uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { getuid() }
}

/// Take over the listening socket launchd created for this agent.
///
/// Fails if the process was not started by launchd, or its property list
/// has no socket.
#[cfg(target_os = "macos")]
pub fn take_listener() -> io::Result<UnixListener> {
    use std::ffi::{c_char, c_int, c_void};
    use std::os::fd::FromRawFd;

    extern "C" {
        // <launch.h>: on success stores a malloc'd array of descriptors.
        fn launch_activate_socket(
            name: *const c_char,
            fds: *mut *mut c_int,
            cnt: *mut usize,
        ) -> c_int;
        fn free(ptr: *mut c_void);
        fn close(fd: c_int) -> c_int;
    }

    let name = std::ffi::CString::new(SOCKET_NAME).expect("no NUL in the socket name");
    let mut fds: *mut c_int = std::ptr::null_mut();
    let mut count: usize = 0;
    // SAFETY: `name` is a valid C string and both out-pointers refer to
    // live locals; launchd fills them only when it returns 0.
    let rc = unsafe { launch_activate_socket(name.as_ptr(), &mut fds, &mut count) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    // SAFETY: on success `fds` points to `count` descriptors that this
    // process now owns, in an array to be released with free().
    let owned: Vec<c_int> = unsafe {
        let v = std::slice::from_raw_parts(fds, count).to_vec();
        free(fds.cast());
        v
    };
    let Some((&first, rest)) = owned.split_first() else {
        return Err(io::Error::other("launchd passed no socket"));
    };
    for &fd in rest {
        // SAFETY: we own these descriptors and use only the first.
        unsafe { close(fd) };
    }
    // SAFETY: `first` is an open listening socket owned by this process.
    let listener = unsafe { UnixListener::from_raw_fd(first) };
    listener.set_nonblocking(false)?;
    Ok(listener)
}

#[cfg(not(target_os = "macos"))]
pub fn take_listener() -> io::Result<UnixListener> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "launchd is only available on macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_names_the_program_socket_and_options() {
        let plist = agent_plist(
            Path::new("/opt/R&D/bin/darwincand"),
            &["--vcan".into(), "vcan0".into(), "--no-hardware".into()],
            Path::new("/Users/me/Library/Application Support/darwincan/darwincand.sock"),
            Path::new("/Users/me/Library/Logs/darwincand.log"),
        );
        // The program comes first, escaped, followed by --launchd and the
        // daemon options in order.
        let args: Vec<&str> = plist
            .split("<array>")
            .nth(1)
            .unwrap()
            .split("</array>")
            .next()
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            args,
            vec![
                "<string>/opt/R&amp;D/bin/darwincand</string>",
                "<string>--launchd</string>",
                "<string>--vcan</string>",
                "<string>vcan0</string>",
                "<string>--no-hardware</string>",
            ]
        );
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
        assert!(plist.contains(
            "<key>SockPathName</key>\n      <string>/Users/me/Library/Application Support/darwincan/darwincand.sock</string>"
        ));
        assert_eq!(
            socket_in_plist(&plist),
            Some(PathBuf::from(
                "/Users/me/Library/Application Support/darwincan/darwincand.sock"
            ))
        );
        let odd = agent_plist(
            Path::new("/bin/d"),
            &[],
            Path::new("/tmp/a&b<c>.sock"),
            Path::new("/tmp/log"),
        );
        assert_eq!(
            socket_in_plist(&odd),
            Some(PathBuf::from("/tmp/a&b<c>.sock"))
        );
        assert_eq!(socket_in_plist("<plist/>"), None);
        // 0600, as `bind` sets for a hand-started daemon.
        assert!(plist.contains("<integer>384</integer>"));
        assert_eq!(0o600, 384);
    }
}
