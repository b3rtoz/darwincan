use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use darwincan::{err, Control, Filter, RawSocket, RxFrame, ANY_INTERFACE};
use darwincan_utils::{
    describe_error, fail, getopt, local_time, long_frame, parse_filter, usage, wants_help,
};

const TOOL: &str = "candump";
const USAGE: &str = "\
Usage: candump [options] <interface>[,<filter>...] ...

Print CAN frames received on one or more interfaces. The interface name
'any' stands for every interface the daemon has, and those that appear
while candump runs.

Options:
  -t <type>     Timestamp: a (absolute), d (delta to the previous frame),
                z (from the first frame), A (absolute with date)
  -a            Also show the data as ASCII
  -e            Describe error frames (bus state changes, overruns); also
                receives all of them unless a #<mask> filter is given
  -L            Log format on standard output: (timestamp) interface frame
  -l            Log to the file candump-<date>_<time>.log instead of printing
  -f <file>     Log to this file instead of printing ('-' means -L)
  -s <level>    With -l or -f: 0 keeps printing as well, 2 prints nothing
  -n <count>    Exit after receiving this many frames
  -T <msecs>    Exit after this long without a frame
  -H            Accepted for compatibility: timestamps always come from the
                adapter or the daemon, never from this program

Filters (hex), any of which may match:
  <id>:<mask>   frames where (received_id & mask) == (id & mask)
  <id>~<mask>   frames where (received_id & mask) != (id & mask)
  #<mask>       error frame classes to receive
  j             a frame must match all the filters given, not just one
An 8-digit id selects the 29-bit format.

Examples:
  candump vcan0
  candump -ta can0,123:7FF,400:700 vcan0
  candump -l any
  candump -e can0,0~0,#FFFFFFFF      (error frames only)";

/// can-utils options that have no counterpart here.
const UNSUPPORTED: &str = "NciSr:Dd8x";

/// One `<interface>[,<filter>...]` argument.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    ifname: String,
    /// `None` keeps the default of receiving every data frame.
    filters: Option<Vec<Filter>>,
    err_mask: Option<u32>,
    /// A frame must match every filter given, not just one.
    join: bool,
}

/// The daemon's interfaces by index, as far as it can be asked.
fn interface_names() -> HashMap<u32, String> {
    Control::connect()
        .and_then(|c| c.interfaces())
        .map(|list| list.into_iter().map(|i| (i.index, i.name)).collect())
        .unwrap_or_default()
}

fn parse_target(arg: &str) -> Result<Target, String> {
    let mut parts = arg.split(',');
    let ifname = parts.next().unwrap_or_default();
    if ifname.is_empty() {
        return Err(format!("missing interface name in '{arg}'"));
    }
    let mut filters = Vec::new();
    let mut err_mask = None;
    let mut join = false;
    for part in parts {
        if let Some(mask) = part.strip_prefix('#') {
            err_mask = Some(
                u32::from_str_radix(mask, 16).map_err(|_| format!("invalid error mask: {part}"))?
                    & err::MASK,
            );
        } else if part.eq_ignore_ascii_case("j") {
            join = true;
        } else {
            filters.push(parse_filter(part)?);
        }
    }
    Ok(Target {
        ifname: ifname.to_string(),
        filters: (!filters.is_empty()).then_some(filters),
        err_mask,
        join,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    None,
    Absolute,
    AbsoluteDate,
    Delta,
    Zero,
}

/// Formats timestamps, remembering what delta and zero modes refer to.
struct Stamper {
    mode: Stamp,
    reference: Option<u64>,
}

impl Stamper {
    fn new(mode: Stamp) -> Stamper {
        Stamper {
            mode,
            reference: None,
        }
    }

    /// The timestamp column including its trailing space; empty for none.
    fn format(&mut self, timestamp_ns: u64) -> String {
        let micros = timestamp_ns / 1_000;
        let (secs, frac) = (micros / 1_000_000, micros % 1_000_000);
        match self.mode {
            Stamp::None => String::new(),
            Stamp::Absolute => format!("({secs:010}.{frac:06}) "),
            Stamp::AbsoluteDate => {
                let t = local_time(secs);
                format!(
                    "({:04}-{:02}-{:02} {:02}:{:02}:{:02}.{frac:06}) ",
                    t.year, t.month, t.day, t.hour, t.minute, t.second
                )
            }
            Stamp::Delta | Stamp::Zero => {
                let reference = *self.reference.get_or_insert(micros);
                if self.mode == Stamp::Delta {
                    self.reference = Some(micros);
                }
                let diff = micros.saturating_sub(reference);
                format!("({:03}.{:06}) ", diff / 1_000_000, diff % 1_000_000)
            }
        }
    }
}

/// The line for one frame in the normal view.
fn console_line(stamp: &str, name: &str, width: usize, body: &str) -> String {
    format!(" {stamp} {name:>width$}  {body}")
}

/// The line for one frame in log format.
fn log_line(stamp: &str, name: &str, width: usize, rx: &RxFrame) -> String {
    format!("{stamp}{name:>width$} {}", rx.frame)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let opts = match getopt(args, &format!("t:aeLlf:s:n:T:H{UNSUPPORTED}")) {
        Ok(o) => o,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    if let Some((flag, _)) = opts
        .flags
        .iter()
        .find(|(f, _)| UNSUPPORTED.contains(*f) && *f != ':')
    {
        return fail(TOOL, format!("option -{flag} is not supported on macOS"));
    }

    let stamp = match opts.value('t') {
        None => Stamp::None,
        Some("a") => Stamp::Absolute,
        Some("A") => Stamp::AbsoluteDate,
        Some("d") => Stamp::Delta,
        Some("z") => Stamp::Zero,
        Some(other) => return usage(TOOL, format!("unknown timestamp type: {other}"), USAGE),
    };
    let number = |flag: char, what: &str| -> Result<Option<u64>, String> {
        match opts.value(flag) {
            None => Ok(None),
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or(format!("invalid {what}: {v}")),
        }
    };
    let (mut remaining, timeout) = match (number('n', "frame count"), number('T', "timeout")) {
        (Ok(n), Ok(t)) => (n, t.map(Duration::from_millis)),
        (Err(e), _) | (_, Err(e)) => return usage(TOOL, e, USAGE),
    };
    let ascii = opts.has('a');
    let describe = opts.has('e');

    // Where the log goes: a file (-l, -f) and/or standard output (-L).
    let mut log_stdout = opts.has('L');
    let mut log_name = opts.value('f').map(str::to_string);
    if log_name.as_deref() == Some("-") {
        log_name = None;
        log_stdout = true;
    } else if log_name.is_none() && opts.has('l') {
        let t = local_time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        log_name = Some(format!(
            "candump-{:04}-{:02}-{:02}_{:02}{:02}{:02}.log",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        ));
    }
    let silent = match (opts.value('s'), &log_name) {
        (None, name) => name.is_some(),
        (Some("0"), _) => false,
        (Some("1" | "2"), _) => true,
        (Some(other), _) => return usage(TOOL, format!("invalid silent level: {other}"), USAGE),
    };

    if opts.positional.is_empty() {
        return usage(TOOL, "missing interface", USAGE);
    }
    let mut targets = Vec::new();
    for arg in &opts.positional {
        let target = match parse_target(arg) {
            Ok(t) => t,
            Err(e) => return usage(TOOL, e, USAGE),
        };
        targets.push(target);
    }

    let mut log_file = match &log_name {
        None => None,
        Some(name) => match File::create(name) {
            Ok(f) => {
                if silent {
                    eprintln!("Disabled standard output while logging.");
                } else {
                    eprintln!("Warning: Console output active while logging!");
                }
                eprintln!("Enabling Logfile '{name}'");
                Some(f)
            }
            Err(e) => return fail(TOOL, format!("cannot create {name}: {e}")),
        },
    };

    // One thread per socket feeds a single queue, so frames print in the
    // order they arrive here; their timestamps were taken in the daemon.
    let (tx, rx) = mpsc::channel();
    let mut width = targets.iter().map(|t| t.ifname.len()).max().unwrap_or(0);
    for target in targets {
        let open = || -> darwincan::Result<RawSocket> {
            let sock = RawSocket::open(&target.ifname)?;
            if let Some(filters) = &target.filters {
                sock.set_filters(filters)?;
            }
            if target.join {
                sock.set_join_filters(true)?;
            }
            match (target.err_mask, describe) {
                (Some(mask), _) => sock.set_error_filter(mask)?,
                (None, true) => sock.set_error_filter(err::MASK)?,
                (None, false) => {}
            }
            Ok(sock)
        };
        let sock = match open() {
            Ok(s) => s,
            Err(e) => return fail(TOOL, format!("{}: {e}", target.ifname)),
        };
        let tx = tx.clone();
        // On every interface, the name is looked up from the index each
        // frame carries; an index not seen before is an interface that
        // has appeared since, or since the last look.
        let everywhere = target.ifname == ANY_INTERFACE;
        if everywhere {
            let known = interface_names();
            width = width.max(known.values().map(String::len).max().unwrap_or(0));
        }
        thread::spawn(move || {
            let mut known = HashMap::new();
            loop {
                let result = sock.recv();
                let failed = result.is_err();
                let name = match &result {
                    Ok(rx) if everywhere => {
                        if !known.contains_key(&rx.ifindex) {
                            known.extend(interface_names());
                        }
                        known
                            .get(&rx.ifindex)
                            .cloned()
                            .unwrap_or_else(|| format!("if{}", rx.ifindex))
                    }
                    _ => target.ifname.clone(),
                };
                if tx.send((name, result)).is_err() || failed {
                    return;
                }
            }
        });
    }
    drop(tx);

    let mut console = Stamper::new(stamp);
    // Logs carry absolute time unless the time from the first frame is asked for.
    let mut logged = Stamper::new(if stamp == Stamp::Zero {
        Stamp::Zero
    } else {
        Stamp::Absolute
    });
    let mut indent_sff = false;
    let stdout = io::stdout();
    loop {
        let (name, result) = match timeout {
            None => match rx.recv() {
                Ok(item) => item,
                Err(_) => return ExitCode::FAILURE,
            },
            Some(limit) => match rx.recv_timeout(limit) {
                Ok(item) => item,
                Err(RecvTimeoutError::Timeout) => return ExitCode::SUCCESS,
                Err(RecvTimeoutError::Disconnected) => return ExitCode::FAILURE,
            },
        };
        let name = &name;
        width = width.max(name.len());
        let rx_frame = match result {
            Ok(f) => f,
            Err(e) => return fail(TOOL, format!("{name}: {e}")),
        };
        let frame = &rx_frame.frame;
        // Once a 29-bit identifier has been seen, 11-bit ones line up with it.
        indent_sff |= frame.is_extended();

        let mut lines = Vec::new();
        if log_file.is_some() || log_stdout {
            let line = log_line(
                &logged.format(rx_frame.timestamp_ns),
                name,
                width,
                &rx_frame,
            );
            if let Some(file) = &mut log_file {
                if let Err(e) = writeln!(file, "{line}") {
                    return fail(TOOL, format!("cannot write the log: {e}"));
                }
            }
            if log_stdout && !silent {
                lines.push(line);
            }
        }
        if !log_stdout && !silent {
            let mut line = console_line(
                &console.format(rx_frame.timestamp_ns),
                name,
                width,
                &long_frame(frame, indent_sff, ascii),
            );
            if describe && frame.is_error() {
                line.push_str(&format!("\n\t{}", describe_error(frame)));
            }
            lines.push(line);
        }
        if !lines.is_empty() {
            let mut out = stdout.lock();
            // A closed pipe (e.g. `candump vcan0 | head`) ends the program quietly.
            if writeln!(out, "{}", lines.join("\n"))
                .and_then(|()| out.flush())
                .is_err()
            {
                return ExitCode::SUCCESS;
            }
        }
        if let Some(n) = &mut remaining {
            *n -= 1;
            if *n == 0 {
                return ExitCode::SUCCESS;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darwincan::{Frame, CAN_INV_FILTER};

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("vcan0"),
            Ok(Target {
                ifname: "vcan0".into(),
                filters: None,
                err_mask: None,
                join: false,
            })
        );
        assert_eq!(
            parse_target("can0,123:7FF,400~700,#000000FF"),
            Ok(Target {
                ifname: "can0".into(),
                filters: Some(vec![
                    Filter::new(0x123, 0x7FF),
                    Filter::new(0x400 | CAN_INV_FILTER, 0x700),
                ]),
                err_mask: Some(0xFF),
                join: false,
            })
        );
        let joined = parse_target("can0,100:700,001:001,j").unwrap();
        assert!(joined.join);
        assert_eq!(joined.filters.map(|f| f.len()), Some(2));
        assert!(parse_target(",123:7FF").is_err());
        assert!(parse_target("can0,123").is_err());
        assert!(parse_target("can0,123:7FF,J").unwrap().join);
        assert!(parse_target("can0,#xyz").is_err());
    }

    #[test]
    fn timestamps() {
        let t0 = 1_700_000_000_123_456_789;
        assert_eq!(Stamper::new(Stamp::None).format(t0), "");
        assert_eq!(
            Stamper::new(Stamp::Absolute).format(t0),
            "(1700000000.123456) "
        );
        assert_eq!(
            Stamper::new(Stamp::Absolute).format(5_000),
            "(0000000000.000005) "
        );

        let mut delta = Stamper::new(Stamp::Delta);
        let mut zero = Stamper::new(Stamp::Zero);
        for (t, d, z) in [
            (t0, "(000.000000) ", "(000.000000) "),
            (t0 + 1_500_000, "(000.001500) ", "(000.001500) "),
            (t0 + 2_001_500_000, "(002.000000) ", "(002.001500) "),
        ] {
            assert_eq!(delta.format(t), d);
            assert_eq!(zero.format(t), z);
        }

        let dated = Stamper::new(Stamp::AbsoluteDate).format(t0);
        assert!(dated.starts_with("(2023-11-1"), "{dated}");
        assert!(dated.ends_with(".123456) "), "{dated}");
    }

    #[test]
    fn lines_match_can_utils() {
        let frame: Frame = "123#DEADBEEF".parse().unwrap();
        let rx = RxFrame {
            frame,
            timestamp_ns: 1_700_000_000_123_456_000,
            own: false,
            ifindex: 1,
            dropped: 0,
        };
        let body = long_frame(&frame, false, false);
        assert_eq!(
            console_line("", "vcan0", 5, &body),
            "  vcan0  123   [4]  DE AD BE EF"
        );
        assert_eq!(
            console_line("(1700000000.123456) ", "can0", 5, &body),
            " (1700000000.123456)   can0  123   [4]  DE AD BE EF"
        );
        assert_eq!(
            log_line("(1700000000.123456) ", "vcan0", 5, &rx),
            "(1700000000.123456) vcan0 123#DEADBEEF"
        );
    }
}
