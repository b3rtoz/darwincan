use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use darwincan::{Frame, RawSocket};
use darwincan_utils::{
    check_sent, fail, getopt, long_frame, prefer_precise_timing, usage, wants_help, Pacer,
};

const TOOL: &str = "canplayer";
const USAGE: &str = "\
Usage: canplayer [options] [<write-interface>=<log-interface> ...]

Replay a log written by 'candump -l' (or 'candump -L') with its original
timing.

Options:
  -I <file>     Read the log from this file (default: standard input)
  -l <num>      Replay the log this many times; 'i' repeats forever
                (needs -I; default: 1)
  -t            Ignore the timestamps and send the frames back to back
  -n <count>    Stop after this many frames
  -s <secs>     Do not wait out gaps in the log longer than this
  -x            Do not deliver the frames to other local programs
  -v            Print each frame sent
  -g <ms>       Accepted for compatibility: frames go out at their logged
                time, not on a polling interval

Interface assignments:
  vcan2=can0    send the frames logged on can0 to vcan2
  stdout=can0   print the log lines of can0 instead of sending them
Without assignments every frame goes to the interface it was logged on;
with any, frames of interfaces that are not assigned are skipped.

Lines that do not start with '(' are ignored, as are error frames.

Examples:
  canplayer -I candump-2024-01-31_120000.log
  canplayer -I trace.log -l i -v vcan0=can0
  candump -L can0 | canplayer vcan0=can0";

/// One usable line of a log.
#[derive(Debug, PartialEq, Eq)]
struct Entry<'a> {
    timestamp_ns: u64,
    ifname: &'a str,
    frame: Frame,
}

/// Parse `(seconds.fraction) interface frame`, with six or nine decimal
/// places. `Ok(None)` is a line to skip.
fn parse_line(line: &str) -> Result<Option<Entry<'_>>, String> {
    let Some(rest) = line.strip_prefix('(') else {
        return Ok(None);
    };
    let bad = || format!("incorrect line format in logfile: {}", line.trim_end());
    let (stamp, rest) = rest.split_once(')').ok_or_else(bad)?;
    let (secs, fraction) = stamp.split_once('.').ok_or_else(bad)?;
    let secs: u64 = secs.parse().map_err(|_| bad())?;
    let nanos: u64 = match fraction.len() {
        6 => fraction.parse::<u64>().map_err(|_| bad())? * 1_000,
        9 => fraction.parse().map_err(|_| bad())?,
        _ => {
            return Err(format!(
                "timestamp format in logfile requires 6 or 9 decimal places: {}",
                line.trim_end()
            ))
        }
    };
    let mut words = rest.split_whitespace();
    let (Some(ifname), Some(text)) = (words.next(), words.next()) else {
        return Err(bad());
    };
    // Error frames are logged with the error flag in an 8-digit id; they
    // describe the bus at the time and cannot be sent.
    if let Some((id, _)) = text.split_once('#') {
        if id.len() == 8 && matches!(u32::from_str_radix(id, 16), Ok(v) if v > 0x1FFF_FFFF) {
            return Ok(None);
        }
    }
    let frame = text
        .parse()
        .map_err(|_| format!("wrong CAN frame format: '{text}'"))?;
    Ok(Some(Entry {
        timestamp_ns: secs * 1_000_000_000 + nanos,
        ifname,
        frame,
    }))
}

/// Where the frames logged on an interface go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Send(String),
    Stdout,
}

fn parse_assignments(args: &[String]) -> Result<HashMap<String, Route>, String> {
    let mut routes = HashMap::new();
    for arg in args {
        let (write, log) = arg
            .split_once('=')
            .filter(|(w, l)| !w.is_empty() && !l.is_empty())
            .ok_or(format!(
                "invalid assignment '{arg}' (expected <write-interface>=<log-interface>)"
            ))?;
        let route = if write == "stdout" {
            Route::Stdout
        } else {
            Route::Send(write.to_string())
        };
        routes.insert(log.to_string(), route);
    }
    Ok(routes)
}

/// Turns log timestamps into send times, keeping the logged spacing.
struct Schedule {
    /// A point in time and the log timestamp that falls on it.
    base: Option<(Instant, u64)>,
    last: u64,
    skip_gap_ns: Option<u64>,
}

impl Schedule {
    fn new(skip_gap: Option<Duration>) -> Schedule {
        Schedule {
            base: None,
            last: 0,
            skip_gap_ns: skip_gap.map(|d| d.as_nanos() as u64),
        }
    }

    /// When to send the frame logged at `timestamp_ns`, if asked at `now`.
    fn due(&mut self, timestamp_ns: u64, now: Instant) -> Instant {
        let long_gap = self
            .skip_gap_ns
            .is_some_and(|limit| timestamp_ns.saturating_sub(self.last) > limit);
        let base = match self.base {
            // Start over at the first frame, when the log jumps back in
            // time, and after a gap the user does not want to sit through.
            Some(base) if timestamp_ns >= self.last && !long_gap => base,
            _ => (now, timestamp_ns),
        };
        self.base = Some(base);
        self.last = timestamp_ns;
        base.0 + Duration::from_nanos(timestamp_ns.saturating_sub(base.1))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let opts = match getopt(args, "I:l:tn:g:s:xvi") {
        Ok(o) => o,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    if opts.has('i') {
        return fail(TOOL, "option -i is not supported on macOS");
    }

    let parsed = (|| -> Result<_, String> {
        let number = |flag: char, what: &str| match opts.value(flag) {
            None => Ok(None),
            Some(v) => v
                .parse::<u64>()
                .map(Some)
                .map_err(|_| format!("invalid {what}: {v}")),
        };
        let loops = match opts.value('l') {
            None => Some(1),
            Some("i") => None,
            Some(v) => Some(
                v.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(format!("invalid loop count: {v}"))?,
            ),
        };
        if loops != Some(1) && opts.value('I').is_none() {
            return Err("-l needs a log file (-I)".into());
        }
        number('g', "gap")?;
        Ok((
            loops,
            number('n', "frame count")?.filter(|n| *n > 0),
            number('s', "gap to skip")?
                .filter(|s| *s > 0)
                .map(Duration::from_secs),
            parse_assignments(&opts.positional)?,
        ))
    })();
    let (loops, mut remaining, skip_gap, routes) = match parsed {
        Ok(p) => p,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let use_timestamps = !opts.has('t');
    let verbose = opts.has('v');
    let assigned = !routes.is_empty();

    let mut log = String::new();
    let read = match opts.value('I') {
        Some(name) => std::fs::read_to_string(name).map(|text| log = text),
        None => io::stdin().read_to_string(&mut log).map(|_| ()),
    };
    if let Err(e) = read {
        return fail(TOOL, format!("cannot read the log: {e}"));
    }

    // Replaying at the logged times wants punctual wake-ups; sending back
    // to back never sleeps and must stay an ordinary thread.
    if use_timestamps {
        prefer_precise_timing();
    }
    let mut pacer = Pacer::calibrated();
    // One sending socket per write interface, opened on first use.
    let mut socks: HashMap<String, RawSocket> = HashMap::new();
    let check_all = |socks: &HashMap<String, RawSocket>| -> Result<(), String> {
        for (name, sock) in socks {
            check_sent(sock).map_err(|e| format!("{name}: {e}"))?;
        }
        Ok(())
    };
    let stdout = io::stdout();
    let mut unchecked = 0u32;
    let mut pass = 0;
    while loops.map_or(true, |n| pass < n) {
        pass += 1;
        let mut schedule = Schedule::new(skip_gap);
        for line in log.lines() {
            let entry = match parse_line(line) {
                Ok(Some(e)) => e,
                Ok(None) => continue,
                Err(e) => return fail(TOOL, e),
            };
            let route = match routes.get(entry.ifname) {
                Some(r) => r.clone(),
                None if assigned => continue,
                None => Route::Send(entry.ifname.to_string()),
            };
            if use_timestamps {
                let now = Instant::now();
                let due = schedule.due(entry.timestamp_ns, now);
                // Use a pause in the log to check on what was sent.
                if due.saturating_duration_since(now) > Duration::from_millis(1) || unchecked >= 256
                {
                    unchecked = 0;
                    if let Err(e) = check_all(&socks) {
                        return fail(TOOL, e);
                    }
                }
                pacer.sleep_until(due);
            } else if unchecked >= 256 {
                unchecked = 0;
                if let Err(e) = check_all(&socks) {
                    return fail(TOOL, e);
                }
            }

            let target = match route {
                Route::Stdout => {
                    if writeln!(stdout.lock(), "{line}").is_err() {
                        return ExitCode::SUCCESS;
                    }
                    continue;
                }
                Route::Send(name) => name,
            };
            if !socks.contains_key(&target) {
                let open = || -> darwincan::Result<RawSocket> {
                    let sock = RawSocket::open(&target)?;
                    // This socket only sends; what comes back are failures.
                    sock.set_filters(&[])?;
                    if opts.has('x') {
                        sock.set_loopback(false)?;
                    }
                    Ok(sock)
                };
                match open() {
                    Ok(s) => socks.insert(target.clone(), s),
                    Err(e) => return fail(TOOL, format!("{target}: {e}")),
                };
            }
            if let Err(e) = socks[&target].send(&entry.frame) {
                return fail(TOOL, format!("{target}: {e}"));
            }
            unchecked += 1;
            if verbose {
                let text = long_frame(&entry.frame, true, false);
                if writeln!(stdout.lock(), "{target} ({}) {text}", entry.ifname).is_err() {
                    return ExitCode::SUCCESS;
                }
            }
            if let Some(n) = &mut remaining {
                *n -= 1;
                if *n == 0 {
                    return finish(check_all(&socks));
                }
            }
        }
        if socks.is_empty() && !assigned && loops.is_none() {
            // Nothing to replay; do not spin on an empty log forever.
            break;
        }
    }
    finish(check_all(&socks))
}

fn finish(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines() {
        let frame: Frame = "123#DEADBEEF".parse().unwrap();
        assert_eq!(
            parse_line("(1700000000.123456) vcan0 123#DEADBEEF"),
            Ok(Some(Entry {
                timestamp_ns: 1_700_000_000_123_456_000,
                ifname: "vcan0",
                frame,
            }))
        );
        // Nanosecond stamps, padded interface names and a direction suffix.
        assert_eq!(
            parse_line("(0000000001.000000007)  can0 123#DEADBEEF R\n"),
            Ok(Some(Entry {
                timestamp_ns: 1_000_000_007,
                ifname: "can0",
                frame,
            }))
        );
        let fd = parse_line("(1.000000) can0 12345678##1AABB")
            .unwrap()
            .unwrap();
        assert!(fd.frame.is_fd() && fd.frame.is_extended());

        assert_eq!(parse_line("# a comment"), Ok(None));
        assert_eq!(parse_line(""), Ok(None));
        assert_eq!(
            parse_line("(1.000000) can0 20000040#0000000000000000"),
            Ok(None)
        );
        for bad in [
            "(1.000000) can0",
            "(1.000000 can0 123#00",
            "(1) can0 123#00",
            "(1.0) can0 123#00",
            "(x.000000) can0 123#00",
            "(1.000000) can0 123#0",
        ] {
            assert!(parse_line(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn assignments() {
        let routes = parse_assignments(&["vcan2=can0".into(), "stdout=can1".into()]).unwrap();
        assert_eq!(routes["can0"], Route::Send("vcan2".into()));
        assert_eq!(routes["can1"], Route::Stdout);
        assert_eq!(routes.len(), 2);
        assert!(parse_assignments(&["vcan0".into()]).is_err());
        assert!(parse_assignments(&["=can0".into()]).is_err());
    }

    #[test]
    fn schedule_keeps_the_logged_spacing() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let at = |n: u64| 5_000_000_000 + n * 1_000_000;

        let mut s = Schedule::new(None);
        assert_eq!(s.due(at(0), start), start);
        // Later frames stay on the original time base however late we ask.
        assert_eq!(s.due(at(10), start + ms(3)), start + ms(10));
        assert_eq!(s.due(at(250), start + ms(11)), start + ms(250));
        // A log that jumps backwards starts a new time base.
        assert_eq!(s.due(at(100), start + ms(251)), start + ms(251));
        assert_eq!(s.due(at(105), start + ms(252)), start + ms(256));

        let mut s = Schedule::new(Some(Duration::from_secs(1)));
        assert_eq!(s.due(at(0), start), start);
        assert_eq!(s.due(at(900), start), start + ms(900));
        // More than a second of silence in the log is not waited out.
        assert_eq!(s.due(at(5000), start + ms(901)), start + ms(901));
        assert_eq!(s.due(at(5020), start + ms(902)), start + ms(921));
    }
}
