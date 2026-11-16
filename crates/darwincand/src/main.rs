// SPDX-License-Identifier: GPL-2.0-or-later

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use darwincan_core::default_socket_path;
use darwincand::{launchd, Daemon};

const USAGE: &str = "\
Usage: darwincand [options]
       darwincand install [--print] [options]
       darwincand uninstall

Run the CAN daemon, or install it as a launch agent that starts at login.

Options:
  --socket <path>   Listen on this Unix socket
                    (default: $DARWINCAN_SOCKET, else
                    ~/Library/Application Support/darwincan/darwincand.sock)
  --vcan <name>     Create a virtual CAN interface; may be repeated
                    (default: vcan0)
  --no-vcan         Do not create the default vcan0
  --no-hardware     Do not look for USB CAN adapters
  --names <file>    Keep the names of hardware interfaces in this file,
                    so that each adapter channel is called the same every
                    time (default: interfaces, beside the default socket)
  --timing-log <ms> Log whatever holds a frame up for longer than this:
                    an adapter slow to take it, or the daemon's own
                    threads being woken late (for finding timing faults)
  --launchd         Serve the socket launchd listens on for the launch
                    agent (set by 'install'; not for use by hand)
  -h, --help        Show this help
  -V, --version     Show the version

Commands:
  install           Register this executable as a launch agent with the
                    given options and start it. launchd owns the socket, so
                    the tools work from login on without starting anything.
                    --print writes the property list to standard output
                    instead of installing it.
  uninstall         Stop the launch agent and remove it";

struct Options {
    socket: Option<PathBuf>,
    vcans: Vec<String>,
    default_vcan: bool,
    hardware: bool,
    names: Option<PathBuf>,
    timing_log: Option<f64>,
    launchd: bool,
    print: bool,
}

/// Parse daemon options. `Ok(None)` means help or version was printed.
fn parse_options(args: &[String], installing: bool) -> Result<Option<Options>, String> {
    let mut o = Options {
        socket: None,
        vcans: Vec::new(),
        default_vcan: true,
        hardware: true,
        names: None,
        timing_log: None,
        launchd: false,
        print: false,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                o.socket = Some(PathBuf::from(args.next().ok_or("--socket needs a path")?));
            }
            "--vcan" => o
                .vcans
                .push(args.next().ok_or("--vcan needs a name")?.clone()),
            "--no-vcan" => o.default_vcan = false,
            "--no-hardware" => o.hardware = false,
            "--names" => {
                o.names = Some(PathBuf::from(
                    args.next().ok_or("--names needs a file name")?,
                ))
            }
            "--timing-log" => {
                let ms = args
                    .next()
                    .ok_or("--timing-log needs a time in milliseconds")?;
                o.timing_log = match ms.parse::<f64>() {
                    Ok(ms) if ms > 0.0 && ms.is_finite() => Some(ms),
                    _ => return Err(format!("--timing-log: not a time in milliseconds: {ms}")),
                };
            }
            "--launchd" if !installing => o.launchd = true,
            "--print" if installing => o.print = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("darwincand {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    if o.launchd && o.socket.is_some() {
        return Err("--socket has no effect with --launchd; launchd owns the socket".into());
    }
    Ok(Some(o))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.first().map(String::as_str) {
        Some(c @ ("install" | "uninstall")) => (c, &args[1..]),
        _ => ("run", &args[..]),
    };
    if command == "uninstall" {
        return match rest {
            [] => report(uninstall()),
            _ => usage_error("uninstall takes no options"),
        };
    }
    let options = match parse_options(rest, command == "install") {
        Ok(Some(o)) => o,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => return usage_error(&e),
    };
    match command {
        "install" => report(install(&options)),
        _ => run(options),
    }
}

fn run(options: Options) -> ExitCode {
    let mut vcans = options.vcans;
    if vcans.is_empty() && options.default_vcan {
        vcans.push("vcan0".into());
    }

    let daemon = Daemon::new();
    // Before anything starts a thread, so that every thread leaves the
    // stop signals to the one that handles them.
    if let Err(e) = daemon.stop_on_signal() {
        eprintln!("darwincand: adapters will stay on the bus if the daemon is killed: {e}");
    }
    if let Some(ms) = options.timing_log {
        if let Err(e) = darwincand::timing::watch(std::time::Duration::from_secs_f64(ms / 1000.0)) {
            eprintln!("darwincand: cannot watch timing: {e}");
        }
    }
    // Before any interface is made, so that a virtual one cannot take a
    // name kept for an adapter.
    if options.hardware {
        let kept = options.names.clone().or_else(|| {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home).join("Library/Application Support/darwincan/interfaces")
            })
        });
        match kept {
            Some(path) => daemon.keep_names_in(&path),
            None => eprintln!(
                "darwincand: no home directory to keep interface names in; \
                 they may change from one start to the next (see --names)"
            ),
        }
    }
    for name in &vcans {
        if let Err(e) = daemon.add_vcan(name) {
            eprintln!("darwincand: cannot create {name}: {e}");
            return ExitCode::FAILURE;
        }
    }

    let (listener, place) = if options.launchd {
        match launchd::take_listener() {
            Ok(l) => (l, "the launchd socket".to_string()),
            Err(e) => {
                eprintln!(
                    "darwincand: cannot take the socket from launchd: {e} \
                     (--launchd only works in the agent set up by 'darwincand install')"
                );
                return ExitCode::FAILURE;
            }
        }
    } else {
        let path = options.socket.unwrap_or_else(default_socket_path);
        match darwincand::bind(&path) {
            Ok(l) => (l, path.display().to_string()),
            Err(e) => {
                eprintln!("darwincand: cannot listen on {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        }
    };
    eprintln!(
        "darwincand {}: listening on {place} (interfaces: {})",
        env!("CARGO_PKG_VERSION"),
        if vcans.is_empty() {
            "none".to_string()
        } else {
            vcans.join(", ")
        }
    );

    if options.hardware {
        if let Err(e) = darwincand::backends::start_hotplug(daemon.clone()) {
            eprintln!("darwincand: cannot watch for USB adapters: {e}");
        }
        if let Err(e) = daemon.restore_after_sleep() {
            eprintln!("darwincand: cannot watch for wake from sleep: {e}");
        }
    }

    match daemon.serve(listener) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("darwincand: {e}");
            ExitCode::FAILURE
        }
    }
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

fn launchctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run launchctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn create_parent(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(dir) => std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display())),
        None => Ok(()),
    }
}

fn install(options: &Options) -> Result<(), String> {
    let home = home()?;
    let program = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("cannot find this executable: {e}"))?;
    let socket = options.socket.clone().unwrap_or_else(default_socket_path);
    if !socket.is_absolute() {
        return Err("the launch agent needs an absolute --socket path".into());
    }
    let log = launchd::log_path(&home);

    // The agent's own arguments: everything but the socket, which launchd
    // creates and hands over.
    let mut extra = Vec::new();
    for name in &options.vcans {
        extra.extend(["--vcan".to_string(), name.clone()]);
    }
    if !options.default_vcan {
        extra.push("--no-vcan".into());
    }
    if !options.hardware {
        extra.push("--no-hardware".into());
    }
    if let Some(names) = &options.names {
        extra.extend(["--names".to_string(), names.display().to_string()]);
    }
    if let Some(ms) = options.timing_log {
        extra.extend(["--timing-log".to_string(), ms.to_string()]);
    }
    let plist = launchd::agent_plist(&program, &extra, &socket, &log);
    if options.print {
        print!("{plist}");
        return Ok(());
    }

    let plist_path = launchd::plist_path(&home);
    for path in [&plist_path, &socket, &log] {
        create_parent(path)?;
    }
    let domain = format!("gui/{}", launchd::uid());
    // Replace an agent that is already loaded; failure means there was none.
    let _ = launchctl(&["bootout", &format!("{domain}/{}", launchd::LABEL)]);
    std::fs::write(&plist_path, plist)
        .map_err(|e| format!("cannot write {}: {e}", plist_path.display()))?;
    launchctl(&["bootstrap", &domain, &plist_path.to_string_lossy()])?;

    println!("Installed launch agent {}", launchd::LABEL);
    println!("  program  {}", program.display());
    println!("  socket   {}", socket.display());
    println!("  log      {}", log.display());
    println!("  plist    {}", plist_path.display());
    println!(
        "The daemon starts at login. Run 'darwincand install' again after moving the executable."
    );
    Ok(())
}

fn uninstall() -> Result<(), String> {
    let plist_path = launchd::plist_path(&home()?);
    let target = format!("gui/{}/{}", launchd::uid(), launchd::LABEL);
    let socket = std::fs::read_to_string(&plist_path)
        .ok()
        .and_then(|plist| launchd::socket_in_plist(&plist));
    let stopped = launchctl(&["bootout", &target]).is_ok();
    // launchd leaves its socket file behind; nothing listens on it now.
    if let Some(socket) = socket {
        if std::os::unix::net::UnixStream::connect(&socket).is_err() {
            let _ = std::fs::remove_file(&socket);
        }
    }
    let removed = match std::fs::remove_file(&plist_path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("cannot remove {}: {e}", plist_path.display())),
    };
    if stopped || removed {
        println!("Removed launch agent {}", launchd::LABEL);
    } else {
        println!("Launch agent {} was not installed", launchd::LABEL);
    }
    Ok(())
}

fn report(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("darwincand: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("darwincand: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}
