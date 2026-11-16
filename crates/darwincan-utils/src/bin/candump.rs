// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use darwincan::{err, Control, Filter, RawSocket, RxFrame, ANY_INTERFACE, CANFD_ESI};
use darwincan_utils::{
    describe_error, fail, getopt, local_time, long_frame_view, parse_filter, usage, wants_help,
    DataView,
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
  -c            Colour: once for the interface name, twice for the line,
                three times for the timestamp as well
  -i            Show the data in binary (lines may exceed 80 characters)
  -S            Show the data bytes in reverse order, marked with '`'
  -x            Show whether a frame was sent from this machine (TX) or
                came from the bus (RX), and the BRS and ESI bits of a
                CAN FD frame
  -d            Report frames this program lost by reading too slowly
  -D            Do not exit when an interface goes away; wait for it
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

/// can-utils options that have no counterpart here: -r sets the size of
/// a socket buffer that does not exist, and -8 shows a raw DLC, which the
/// daemon does not carry.
const UNSUPPORTED: &str = "Nr:8";

const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
/// The colours interfaces get, in the order they are named.
const COLOURS: [&str; 6] = [
    "\x1b[34m", "\x1b[31m", "\x1b[32m", "", "\x1b[35m", "\x1b[36m",
];

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

/// The line for one frame in the normal view, coloured to `level` (0 to
/// 3) for the interface that is number `nth` of those shown.
fn coloured_line(
    stamp: &str,
    name: &str,
    width: usize,
    body: &str,
    level: usize,
    nth: usize,
) -> String {
    let colour = format!("{BOLD}{}", COLOURS[nth % COLOURS.len()]);
    match level {
        0 => format!(" {stamp} {name:>width$}  {body}"),
        1 => format!(" {stamp} {colour}{name:>width$}{RESET}  {body}"),
        2 => format!(" {stamp} {colour}{name:>width$}  {body}{RESET}"),
        _ => format!(" {colour}{stamp} {name:>width$}  {body}{RESET}"),
    }
}

/// What `-x` puts in front of a frame: where it came from, and the bit
/// rate switch and error state bits of a CAN FD frame.
fn extra_info(rx: &RxFrame) -> String {
    format!(
        "{} {} {}  ",
        if rx.local { "TX" } else { "RX" },
        if rx.frame.is_brs() { "B" } else { "-" },
        if rx.frame.is_fd() && rx.frame.flags() & CANFD_ESI != 0 {
            "E"
        } else {
            "-"
        },
    )
}

/// The line for one frame in log format; with `extra`, followed by where
/// the frame came from.
fn log_line(stamp: &str, name: &str, width: usize, rx: &RxFrame, extra: bool) -> String {
    let origin = match (extra, rx.local) {
        (false, _) => "",
        (true, true) => " T",
        (true, false) => " R",
    };
    format!("{stamp}{name:>width$} {}{origin}", rx.frame)
}

/// What `-d` prints when frames were lost since the last one shown.
fn drop_line(lost: u64, name: &str, total: u64) -> String {
    format!(
        "DROPCOUNT: dropped {lost} CAN frame{} on '{name}' socket (total drops {total})",
        if lost == 1 { "" } else { "s" }
    )
}

/// Open the socket for one interface argument.
fn open_target(target: &Target, describe: bool) -> darwincan::Result<RawSocket> {
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
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let opts = match getopt(args, &format!("t:aeLlf:s:n:T:HciSxdD{UNSUPPORTED}")) {
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
    let view = DataView {
        ascii: opts.has('a'),
        binary: opts.has('i'),
        swap: opts.has('S'),
    };
    let describe = opts.has('e');
    let colour = opts.count('c');
    let extra = opts.has('x');
    let monitor_drops = opts.has('d');
    let stay = opts.has('D');

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
    let everywhere_any = targets.iter().any(|t| t.ifname == ANY_INTERFACE);
    let (tx, rx) = mpsc::channel();
    let mut width = targets.iter().map(|t| t.ifname.len()).max().unwrap_or(0);
    // The interfaces in the order they are first named or seen: each has
    // its colour, and its count of lost frames.
    let mut shown: Vec<(String, u64)> = targets
        .iter()
        .filter(|t| t.ifname != ANY_INTERFACE)
        .map(|t| (t.ifname.clone(), 0))
        .collect();
    for target in targets {
        let mut sock = match open_target(&target, describe) {
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
                if result.is_err() && stay {
                    // The interface went away: wait for it to come back
                    // (an adapter plugged in again gets its name back).
                    loop {
                        thread::sleep(Duration::from_millis(500));
                        if let Ok(again) = open_target(&target, describe) {
                            sock = again;
                            break;
                        }
                    }
                    continue;
                }
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
        let nth = match shown.iter().position(|(n, _)| n == name) {
            Some(nth) => nth,
            None => {
                shown.push((name.clone(), 0));
                shown.len() - 1
            }
        };

        let mut lines = Vec::new();
        if monitor_drops {
            // The count is the socket's: on every interface at once it
            // is one count, told with whichever frame comes next.
            let total = rx_frame.dropped;
            let before = shown.iter().map(|(_, lost)| *lost).max().unwrap_or(0);
            let told = if everywhere_any { before } else { shown[nth].1 };
            if total > told {
                lines.push(drop_line(total - told, name, total));
                if everywhere_any {
                    shown.iter_mut().for_each(|(_, lost)| *lost = total);
                } else {
                    shown[nth].1 = total;
                }
            }
        }
        if log_file.is_some() || log_stdout {
            let line = log_line(
                &logged.format(rx_frame.timestamp_ns),
                name,
                width,
                &rx_frame,
                extra,
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
            let mut body = if extra {
                extra_info(&rx_frame)
            } else {
                String::new()
            };
            body.push_str(&long_frame_view(frame, indent_sff, view));
            let mut line = coloured_line(
                &console.format(rx_frame.timestamp_ns),
                name,
                width,
                &body,
                colour,
                nth,
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

    /// The line for one frame without colour.
    fn plain_line(stamp: &str, name: &str, width: usize, body: &str) -> String {
        coloured_line(stamp, name, width, body, 0, 0)
    }

    #[test]
    fn lines_match_can_utils() {
        let frame: Frame = "123#DEADBEEF".parse().unwrap();
        let rx = RxFrame {
            frame,
            timestamp_ns: 1_700_000_000_123_456_000,
            own: false,
            local: true,
            ifindex: 1,
            dropped: 0,
        };
        let body = long_frame_view(&frame, false, DataView::default());
        assert_eq!(
            plain_line("", "vcan0", 5, &body),
            "  vcan0  123   [4]  DE AD BE EF"
        );
        assert_eq!(
            plain_line("(1700000000.123456) ", "can0", 5, &body),
            " (1700000000.123456)   can0  123   [4]  DE AD BE EF"
        );
        assert_eq!(
            log_line("(1700000000.123456) ", "vcan0", 5, &rx, false),
            "(1700000000.123456) vcan0 123#DEADBEEF"
        );
        // With -x: where the frame came from, and the CAN FD bits.
        assert_eq!(
            log_line("(1700000000.123456) ", "vcan0", 5, &rx, true),
            "(1700000000.123456) vcan0 123#DEADBEEF T"
        );
        let with = |text: &str, local: bool| {
            let frame: Frame = text.parse().unwrap();
            let rx = RxFrame { frame, local, ..rx };
            format!(
                "{}{}",
                extra_info(&rx),
                long_frame_view(&frame, true, DataView::default())
            )
        };
        assert_eq!(
            plain_line("", "vcan9", 5, &with("123#DEADBEEF", true)),
            "  vcan9  TX - -       123   [4]  DE AD BE EF"
        );
        assert_eq!(
            plain_line("", "vcan9", 5, &with("123##3AA", true)),
            "  vcan9  TX B E       123  [01]  AA"
        );
        assert_eq!(
            plain_line("", "vcan9", 5, &with("123##0AA", false)),
            "  vcan9  RX - -       123  [01]  AA"
        );
    }

    #[test]
    fn colours_and_drop_counts_match_can_utils() {
        let body = "123   [1]  01";
        // Once: the interface name, in its interface's colour.
        assert_eq!(
            coloured_line("", "vc1", 3, body, 1, 0),
            "  \x1b[1m\x1b[34mvc1\x1b[0m  123   [1]  01"
        );
        assert_eq!(
            coloured_line("", "vc2", 3, body, 1, 1),
            "  \x1b[1m\x1b[31mvc2\x1b[0m  123   [1]  01"
        );
        // The fourth is only bold, and the seventh starts over.
        assert_eq!(
            coloured_line("", "vc4", 3, body, 1, 3),
            "  \x1b[1mvc4\x1b[0m  123   [1]  01"
        );
        assert_eq!(
            coloured_line("", "vc7", 3, body, 1, 6),
            "  \x1b[1m\x1b[34mvc7\x1b[0m  123   [1]  01"
        );
        // Twice: the line. Three times: the timestamp too.
        assert_eq!(
            coloured_line("", "vc1", 3, body, 2, 0),
            "  \x1b[1m\x1b[34mvc1  123   [1]  01\x1b[0m"
        );
        assert_eq!(
            coloured_line("(1794854338.295894) ", "vc1", 3, body, 3, 0),
            " \x1b[1m\x1b[34m(1794854338.295894)  vc1  123   [1]  01\x1b[0m"
        );
        assert_eq!(
            drop_line(2991, "vc1", 2991),
            "DROPCOUNT: dropped 2991 CAN frames on 'vc1' socket (total drops 2991)"
        );
        assert_eq!(
            drop_line(1, "can0", 5),
            "DROPCOUNT: dropped 1 CAN frame on 'can0' socket (total drops 5)"
        );
    }
}
