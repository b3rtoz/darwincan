//! Helpers shared by the command-line tools.

use std::fmt::Display;
use std::process::ExitCode;

use darwincan::{
    err, Filter, Frame, IsoTpParams, CAN_EFF_FLAG, CAN_EFF_MASK, CAN_ERR_FLAG, CAN_INV_FILTER,
    CAN_SFF_MASK,
};

/// Print `tool: message` to stderr and return a failure exit code.
pub fn fail(tool: &str, message: impl Display) -> ExitCode {
    eprintln!("{tool}: {message}");
    ExitCode::FAILURE
}

/// Print a usage error followed by the usage text; exit code 2.
pub fn usage(tool: &str, message: impl Display, usage: &str) -> ExitCode {
    eprintln!("{tool}: {message}\n\n{usage}");
    ExitCode::from(2)
}

/// Parse a hex CAN identifier as can-utils does: more than 7 digits (or a
/// value above 0x7FF) means a 29-bit identifier.
pub fn parse_can_id(s: &str) -> Result<u32, String> {
    let v = u32::from_str_radix(s, 16).map_err(|_| format!("invalid CAN id: {s}"))?;
    if s.len() > 7 || v > CAN_SFF_MASK {
        if v > CAN_EFF_MASK {
            return Err(format!("CAN id out of range: {s}"));
        }
        Ok(v | CAN_EFF_FLAG)
    } else {
        Ok(v)
    }
}

/// Parse whitespace-separated hex bytes (`"11 22 0a"`).
pub fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    s.split_whitespace()
        .map(|tok| u8::from_str_radix(tok, 16).map_err(|_| format!("invalid hex byte: {tok}")))
        .collect()
}

pub fn hex_bytes(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 3);
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{b:02X}"));
    }
    out
}

/// Command-line options split the way `getopt(3)` does it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Opts {
    /// Options in the order given, with their value if they take one.
    pub flags: Vec<(char, Option<String>)>,
    /// Everything that is not an option.
    pub positional: Vec<String>,
}

impl Opts {
    pub fn has(&self, flag: char) -> bool {
        self.flags.iter().any(|(f, _)| *f == flag)
    }

    /// How many times a value-less option was given (`-v -v`).
    pub fn count(&self, flag: char) -> usize {
        self.flags.iter().filter(|(f, _)| *f == flag).count()
    }

    /// The value of the last occurrence of an option.
    pub fn value(&self, flag: char) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| *f == flag)
            .and_then(|(_, v)| v.as_deref())
    }
}

/// Split arguments like GNU `getopt`: `spec` lists the option letters, each
/// followed by `:` if it takes a value. Options may be clustered (`-ex`),
/// carry their value attached (`-ta`, `-n5`) or separate (`-n 5`), and may
/// come before or after the other arguments; `--` ends them.
pub fn getopt(args: impl IntoIterator<Item = String>, spec: &str) -> Result<Opts, String> {
    let takes_value = |flag: char| -> Result<bool, String> {
        match spec.find(flag) {
            Some(i) if flag != ':' => Ok(spec[i + flag.len_utf8()..].starts_with(':')),
            _ => Err(format!("unknown option: -{flag}")),
        }
    };
    let mut opts = Opts::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            opts.positional.extend(args);
            break;
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) else {
            opts.positional.push(arg);
            continue;
        };
        let mut chars = cluster.chars();
        while let Some(flag) = chars.next() {
            if !takes_value(flag)? {
                opts.flags.push((flag, None));
                continue;
            }
            let attached = chars.as_str();
            let value = if attached.is_empty() {
                args.next().ok_or(format!("-{flag} needs a value"))?
            } else {
                attached.to_string()
            };
            opts.flags.push((flag, Some(value)));
            break;
        }
    }
    Ok(opts)
}

/// True if the arguments ask for help.
pub fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "-h" || a == "--help" || a == "-?")
}

/// Parse one `candump`-style filter: `<id>:<mask>` or `<id>~<mask>` in hex.
/// An 8-digit id selects the 29-bit format, as in can-utils.
pub fn parse_filter(s: &str) -> Result<Filter, String> {
    let (id, mask, invert) = if let Some((id, mask)) = s.split_once(':') {
        (id, mask, false)
    } else if let Some((id, mask)) = s.split_once('~') {
        (id, mask, true)
    } else {
        return Err(format!("invalid filter: {s}"));
    };
    let parse = |v: &str| u32::from_str_radix(v, 16).map_err(|_| format!("invalid filter: {s}"));
    let mut can_id = parse(id)?;
    if id.len() == 8 {
        can_id |= CAN_EFF_FLAG;
    }
    if invert {
        can_id |= CAN_INV_FILTER;
    }
    Ok(Filter::new(can_id, parse(mask)?))
}

/// A frame in the column layout `candump` prints: identifier, `[length]`,
/// data bytes. `indent_sff` right-aligns 11-bit identifiers with 29-bit
/// ones; `ascii` appends the payload as text (payloads up to 8 bytes).
pub fn long_frame(f: &Frame, indent_sff: bool, ascii: bool) -> String {
    let mut s = if f.is_error() {
        format!("{:08X}", f.can_id() & (CAN_ERR_FLAG | err::MASK))
    } else if f.is_extended() {
        format!("{:08X}", f.id().raw())
    } else if indent_sff {
        format!("     {:03X}", f.id().raw())
    } else {
        format!("{:03X}", f.id().raw())
    };
    if f.is_fd() {
        s.push_str(&format!("  [{:02}]", f.len()));
    } else {
        s.push_str(&format!("   [{}]", f.len()));
    }
    if f.is_remote() {
        s.push_str("  remote request");
        return s;
    }
    s.push(' ');
    for b in f.data() {
        s.push_str(&format!(" {b:02X}"));
    }
    if f.len() > 8 {
        return s;
    }
    // Text after the data starts in the same column whatever the length.
    let pad = 3 * (8 - f.len());
    if f.is_error() {
        s.push_str(&format!("{:>1$}", "ERRORFRAME", pad + 13));
    } else if ascii {
        s.push_str(&format!("{:>1$}", "'", pad + 4));
        s.extend(f.data().iter().map(|&b| {
            if (0x20..0x7F).contains(&b) {
                b as char
            } else {
                '.'
            }
        }));
        s.push('\'');
    }
    s
}

/// A frame in log notation with `.` between the data bytes
/// (`123#DE.AD`, `123##1.DE.AD`), as `cangen -v` prints it.
pub fn dotted_frame(f: &Frame) -> String {
    let compact = f.to_string();
    if f.is_remote() || f.data().is_empty() {
        return compact;
    }
    let hex_len = f.data().len() * 2;
    let (head, hex) = compact.split_at(compact.len() - hex_len);
    let mut s = head.to_string();
    if f.is_fd() {
        s.push('.');
    }
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        if i > 0 {
            s.push('.');
        }
        s.push_str(std::str::from_utf8(pair).expect("hex digits are ASCII"));
    }
    s
}

/// Spell out what an error frame reports.
pub fn describe_error(f: &Frame) -> String {
    let class = f.can_id() & err::MASK;
    let d = f.data();
    let mut parts = Vec::new();
    if class & err::BUSOFF != 0 {
        parts.push("bus off".to_string());
    }
    if class & err::RESTARTED != 0 {
        parts.push("restarted".to_string());
    }
    if class & err::CRTL != 0 {
        for (bit, text) in [
            (err::CRTL_RX_OVERFLOW, "receive overrun"),
            (err::CRTL_RX_WARNING, "error warning (receive)"),
            (err::CRTL_TX_WARNING, "error warning (transmit)"),
            (err::CRTL_RX_PASSIVE, "error passive (receive)"),
            (err::CRTL_TX_PASSIVE, "error passive (transmit)"),
            (err::CRTL_ACTIVE, "error active"),
        ] {
            if d[1] & bit != 0 {
                parts.push(text.to_string());
            }
        }
    }
    if class & err::PROT != 0 {
        parts.push("protocol violation".to_string());
    }
    if class & err::CNT != 0 {
        parts.push(format!("error counters tx {} rx {}", d[6], d[7]));
    }
    if parts.is_empty() {
        parts.push(format!("class {class:#x}"));
    }
    parts.join(", ")
}

/// A calendar date and time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Convert seconds since the Unix epoch to local time.
pub fn local_time(unix_secs: u64) -> DateTime {
    use std::ffi::{c_char, c_int, c_long};

    // `struct tm` as in <time.h> on macOS and other BSD-derived libcs.
    #[repr(C)]
    struct Tm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        tm_mday: c_int,
        tm_mon: c_int,
        tm_year: c_int,
        tm_wday: c_int,
        tm_yday: c_int,
        tm_isdst: c_int,
        tm_gmtoff: c_long,
        tm_zone: *const c_char,
    }
    extern "C" {
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    }

    let time = unix_secs as i64;
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    // SAFETY: both pointers refer to live locals of the types the C
    // function expects (time_t is 64-bit here); a zeroed `struct tm` is
    // valid, and it is only read after localtime_r reports success.
    let tm = unsafe {
        if localtime_r(&time, tm.as_mut_ptr()).is_null() {
            return DateTime {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
            };
        }
        tm.assume_init()
    };
    DateTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    }
}

/// Put the calling thread in the real-time scheduling class, for tools
/// that send frames at set times.
///
/// macOS wakes an ordinary thread from a sleep late, by a millisecond on
/// average and several at worst; a real-time thread is woken within some
/// tens of microseconds. Such a thread must run briefly and then sleep
/// again, so do not use this for sending at full speed. Does nothing on
/// other systems; failure is harmless and ignored.
pub fn prefer_precise_timing() {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::c_void;

        // THREAD_TIME_CONSTRAINT_POLICY in <mach/thread_policy.h>.
        const TIME_CONSTRAINT: u32 = 2;
        extern "C" {
            fn pthread_self() -> *mut c_void;
            fn pthread_mach_thread_np(thread: *mut c_void) -> u32;
            fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
            fn thread_policy_set(
                thread: u32,
                flavor: u32,
                info: *const [u32; 4],
                count: u32,
            ) -> i32;
        }

        // Numerator and denominator of nanoseconds per mach time unit.
        let mut timebase = [0u32; 2];
        // SAFETY: the pointer refers to a live local with the layout of
        // mach_timebase_info_data_t, two 32-bit integers.
        if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase[0] == 0 {
            return;
        }
        let units = |nanos: u64| (nanos * u64::from(timebase[1]) / u64::from(timebase[0])) as u32;
        // Period (none), processor time needed per wake-up, how soon it
        // must have been given, and whether the thread may be preempted.
        let policy = [0, units(400_000), units(1_000_000), 1];
        // SAFETY: the port names the calling thread and `policy` holds the
        // four 32-bit integers the count announces.
        unsafe {
            thread_policy_set(
                pthread_mach_thread_np(pthread_self()),
                TIME_CONSTRAINT,
                &policy,
                4,
            );
        }
    }
}

/// Waits for points in time more precisely than a plain sleep.
///
/// A sleeping thread wakes late: by tens of microseconds if it is
/// real-time (see [`prefer_precise_timing`]), by milliseconds if not. The
/// pacer sleeps until shortly before the deadline and spins the rest, and
/// learns how much "shortly" has to be from how late it was woken, so it
/// stays accurate even where the real-time class is not available.
pub struct Pacer {
    margin: std::time::Duration,
    /// Report on standard error when a wait ends later than this. Set by
    /// `DARWINCAN_PACER_LOG=<milliseconds>`, for finding timing faults.
    report_over: Option<std::time::Duration>,
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new()
    }
}

impl Pacer {
    const MIN_MARGIN: std::time::Duration = std::time::Duration::from_micros(150);
    /// Never spin for longer than this, whatever the scheduler does.
    const MAX_MARGIN: std::time::Duration = std::time::Duration::from_millis(12);

    pub fn new() -> Pacer {
        let report_over = std::env::var("DARWINCAN_PACER_LOG")
            .ok()
            .and_then(|ms| ms.parse::<f64>().ok())
            .filter(|ms| *ms > 0.0 && ms.is_finite())
            .map(|ms| std::time::Duration::from_secs_f64(ms / 1000.0));
        Pacer {
            margin: Self::MIN_MARGIN,
            report_over,
        }
    }

    /// A pacer that has measured a few short sleeps, so that the first
    /// wait is already precise. Takes a few milliseconds.
    pub fn calibrated() -> Pacer {
        use std::time::{Duration, Instant};
        let mut pacer = Pacer::new();
        let nap = Duration::from_micros(500);
        for _ in 0..4 {
            let start = Instant::now();
            std::thread::sleep(nap);
            let overslept = start.elapsed().saturating_sub(nap);
            pacer.margin = pacer.margin.max(pacer.next_margin(overslept));
        }
        pacer
    }

    /// Return at `deadline`, or at once if it has passed.
    pub fn sleep_until(&mut self, deadline: std::time::Instant) {
        use std::time::Instant;
        let now = Instant::now();
        // Late before the wait began: the caller was held up, not the wait.
        self.report(
            now.saturating_duration_since(deadline),
            "the deadline had passed by",
        );
        if deadline.saturating_duration_since(now) > self.margin {
            let wake = deadline - self.margin;
            std::thread::sleep(wake - now);
            let overslept = Instant::now().saturating_duration_since(wake);
            self.margin = self.next_margin(overslept);
        }
        while Instant::now() < deadline {
            std::hint::spin_loop();
        }
        if now < deadline {
            self.report(
                Instant::now().saturating_duration_since(deadline),
                "woken late by",
            );
        }
    }

    fn report(&self, late: std::time::Duration, what: &str) {
        if self.report_over.is_some_and(|limit| late > limit) {
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            eprintln!(
                "pacer {}.{:03}: {what} {:.1} ms",
                at.as_secs(),
                at.subsec_millis(),
                late.as_secs_f64() * 1e3
            );
        }
    }

    /// Grow at once to cover a late wake-up with room to spare; shrink
    /// slowly while wake-ups are punctual.
    fn next_margin(&self, overslept: std::time::Duration) -> std::time::Duration {
        let needed = overslept + overslept / 4 + Self::MIN_MARGIN;
        let next = if needed > self.margin {
            needed
        } else {
            self.margin - (self.margin - needed) / 32
        };
        next.clamp(Self::MIN_MARGIN, Self::MAX_MARGIN)
    }
}

/// Wait for the daemon to handle everything sent on `sock` and report the
/// first transmit failure. For sockets that receive no frames (empty
/// filter list); a frame that does arrive is discarded.
pub fn check_sent(sock: &darwincan::RawSocket) -> darwincan::Result<()> {
    sock.flush()?;
    while sock.try_recv()?.is_some() {}
    Ok(())
}

/// A small pseudo-random generator (xorshift64*), for test traffic only.
pub struct Rng(u64);

impl Rng {
    /// Seeded from the clock and process id, so every run differs.
    pub fn from_clock() -> Rng {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Rng::new(nanos ^ (u64::from(std::process::id()) << 32))
    }

    pub fn new(seed: u64) -> Rng {
        // The generator must not start at zero.
        Rng(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
}

/// Options common to `isotpsend` and `isotprecv`.
pub const ISOTP_OPTIONS: &str = "\
  -s <id>       Source (transmit) CAN id, hex
  -d <id>       Destination (receive) CAN id, hex
  -p <byte>     Pad transmitted frames with this hex byte
  -b <n>        Block size announced in flow control (default 0: unlimited)
  -m <stmin>    STmin announced in flow control, raw hex byte
                (00-7F: milliseconds, F1-F9: 100-900 microseconds)
  -L <mtu>:<tx_dl>:<tx_flags>
                Link layer: mtu 16 (classic CAN) or 72 (CAN FD); tx_dl 8, 12,
                16, 20, 24, 32, 48 or 64; tx_flags 1 sets bit rate switch";

/// The result of parsing ISO-TP tool arguments.
pub struct IsoTpArgs {
    pub ifname: String,
    pub params: IsoTpParams,
    /// Flags with no value that were present (e.g. `'l'`).
    pub switches: Vec<char>,
}

/// Parse `-s`, `-d`, `-p`, `-b`, `-m`, `-L`, the value-less `switches`, and
/// one interface name.
pub fn parse_isotp_args(
    args: impl Iterator<Item = String>,
    switches: &[char],
) -> Result<IsoTpArgs, String> {
    let mut tx_id = None;
    let mut rx_id = None;
    let mut params = IsoTpParams::new(0, 1);
    let mut ifname = None;
    let mut seen = Vec::new();

    let mut args = args;
    while let Some(arg) = args.next() {
        let Some(flag) = arg.strip_prefix('-').filter(|f| f.len() == 1) else {
            if ifname.replace(arg).is_some() {
                return Err("more than one interface given".into());
            }
            continue;
        };
        let flag = flag.chars().next().unwrap();
        if switches.contains(&flag) {
            seen.push(flag);
            continue;
        }
        let mut value = || args.next().ok_or(format!("-{flag} needs a value"));
        let hex_byte =
            |v: String| u8::from_str_radix(&v, 16).map_err(|_| format!("invalid hex byte: {v}"));
        match flag {
            's' => tx_id = Some(parse_can_id(&value()?)?),
            'd' => rx_id = Some(parse_can_id(&value()?)?),
            'p' => params.padding = Some(hex_byte(value()?)?),
            'm' => params.st_min = hex_byte(value()?)?,
            'b' => {
                let v = value()?;
                params.block_size = v.parse().map_err(|_| format!("invalid block size: {v}"))?;
            }
            'L' => {
                let v = value()?;
                let parts: Vec<&str> = v.split(':').collect();
                let bad = || format!("invalid link layer options: {v}");
                if parts.len() != 3 {
                    return Err(bad());
                }
                params.fd = match parts[0] {
                    "16" => false,
                    "72" => true,
                    _ => return Err(bad()),
                };
                params.tx_dl = parts[1].parse().map_err(|_| bad())?;
                let flags: u8 = parts[2].parse().map_err(|_| bad())?;
                params.brs = flags & 1 != 0;
            }
            _ => return Err(format!("unknown option: -{flag}")),
        }
    }

    params.tx_id = tx_id.ok_or("missing -s <id>")?;
    params.rx_id = rx_id.ok_or("missing -d <id>")?;
    params.validate()?;
    Ok(IsoTpArgs {
        ifname: ifname.ok_or("missing interface name")?,
        params,
        switches: seen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> impl Iterator<Item = String> + '_ {
        s.split_whitespace().map(String::from)
    }

    #[test]
    fn can_ids() {
        assert_eq!(parse_can_id("7E0"), Ok(0x7E0));
        assert_eq!(parse_can_id("18DAF110"), Ok(0x18DA_F110 | CAN_EFF_FLAG));
        assert_eq!(parse_can_id("00000123"), Ok(0x123 | CAN_EFF_FLAG));
        assert!(parse_can_id("FFFFFFFF").is_err());
        assert!(parse_can_id("xyz").is_err());
    }

    #[test]
    fn hex() {
        assert_eq!(parse_hex_bytes(" 11 22\n0a "), Ok(vec![0x11, 0x22, 0x0A]));
        assert!(parse_hex_bytes("11 2g").is_err());
        assert_eq!(hex_bytes(&[0x01, 0xAB]), "01 AB");
    }

    #[test]
    fn isotp_args() {
        let a = parse_isotp_args(args("-s 7E0 -d 7E8 -p AA -b 8 -m F5 -l vcan0"), &['l']).unwrap();
        assert_eq!(a.ifname, "vcan0");
        assert_eq!(a.params.tx_id, 0x7E0);
        assert_eq!(a.params.rx_id, 0x7E8);
        assert_eq!(a.params.padding, Some(0xAA));
        assert_eq!(a.params.block_size, 8);
        assert_eq!(a.params.st_min, 0xF5);
        assert_eq!(a.switches, vec!['l']);

        let a = parse_isotp_args(args("vcan0 -s 7E0 -d 7E8 -L 72:64:1"), &[]).unwrap();
        assert!(a.params.fd && a.params.brs);
        assert_eq!(a.params.tx_dl, 64);

        assert!(parse_isotp_args(args("-s 7E0 vcan0"), &[]).is_err());
        assert!(parse_isotp_args(args("-s 7E0 -d 7E8"), &[]).is_err());
        assert!(parse_isotp_args(args("-s 7E0 -d 7E8 -L 16:64:0 vcan0"), &[]).is_err());
        assert!(parse_isotp_args(args("-s 7E0 -d 7E8 -x vcan0"), &[]).is_err());
    }

    #[test]
    fn getopt_splits_like_gnu_getopt() {
        let o = getopt(args("-ta -ex vcan0,123:7FF -n5 can0 -T 100"), "t:exn:T:").unwrap();
        assert_eq!(
            o.flags,
            vec![
                ('t', Some("a".to_string())),
                ('e', None),
                ('x', None),
                ('n', Some("5".to_string())),
                ('T', Some("100".to_string())),
            ]
        );
        assert_eq!(o.positional, vec!["vcan0,123:7FF", "can0"]);
        assert_eq!(o.value('n'), Some("5"));
        assert!(o.has('e') && !o.has('L'));

        let o = getopt(args("-v -v -- -x"), "v").unwrap();
        assert_eq!(o.count('v'), 2);
        assert_eq!(o.positional, vec!["-x"]);

        assert!(getopt(args("-q"), "t:e").is_err());
        assert!(getopt(args("-t"), "t:e").is_err());
        assert!(getopt(args("-:"), "t:e").is_err());
    }

    #[test]
    fn filters() {
        assert_eq!(parse_filter("123:7FF"), Ok(Filter::new(0x123, 0x7FF)));
        assert_eq!(
            parse_filter("400~7F0"),
            Ok(Filter::new(0x400 | CAN_INV_FILTER, 0x7F0))
        );
        assert_eq!(
            parse_filter("12345678:DFFFFFFF"),
            Ok(Filter::new(0x1234_5678 | CAN_EFF_FLAG, 0xDFFF_FFFF))
        );
        assert!(parse_filter("123").is_err());
        assert!(parse_filter("xyz:7FF").is_err());
    }

    #[test]
    fn long_frames_use_the_candump_columns() {
        let f = |s: &str| s.parse::<Frame>().unwrap();
        assert_eq!(
            long_frame(&f("123#DEADBEEF"), false, false),
            "123   [4]  DE AD BE EF"
        );
        assert_eq!(
            long_frame(&f("123#DEADBEEF"), true, false),
            "     123   [4]  DE AD BE EF"
        );
        assert_eq!(
            long_frame(&f("18DAF110#0210"), false, false),
            "18DAF110   [2]  02 10"
        );
        assert_eq!(long_frame(&f("123#"), false, false), "123   [0] ");
        assert_eq!(
            long_frame(&f("123#R4"), false, false),
            "123   [4]  remote request"
        );
        assert_eq!(
            long_frame(&f("123##1112233445566778899"), false, false),
            "123  [12]  11 22 33 44 55 66 77 88 99 00 00 00"
        );
        assert_eq!(
            long_frame(&f("7FF##0AB"), false, true),
            "7FF  [01]  AB                        '.'"
        );
        assert_eq!(
            long_frame(&f("123#48692100"), false, true),
            "123   [4]  48 69 21 00               'Hi!.'"
        );
        assert_eq!(
            long_frame(&f("123#4142434445464748"), false, true),
            "123   [8]  41 42 43 44 45 46 47 48   'ABCDEFGH'"
        );
        assert_eq!(
            long_frame(&Frame::new_error(err::BUSOFF, [0; 8]), false, false),
            "20000040   [8]  00 00 00 00 00 00 00 00   ERRORFRAME"
        );
    }

    #[test]
    fn dotted_frames() {
        let f = |s: &str| dotted_frame(&s.parse::<Frame>().unwrap());
        assert_eq!(f("123#DEADBEEF"), "123#DE.AD.BE.EF");
        assert_eq!(f("123#"), "123#");
        assert_eq!(f("123#R2"), "123#R2");
        assert_eq!(f("12345678##1AABB"), "12345678##1.AA.BB");
    }

    #[test]
    fn local_time_is_a_plausible_calendar_date() {
        // 2023-11-14 22:13:20 UTC; the local date is within a day of it.
        let t = local_time(1_700_000_000);
        assert_eq!((t.year, t.month), (2023, 11));
        assert!((14..=15).contains(&t.day));
        assert!(t.hour < 24 && t.minute < 60 && t.second < 60);
    }

    #[test]
    fn pacer_margin_follows_wake_up_lateness() {
        use std::time::Duration;
        let us = Duration::from_micros;
        let mut p = Pacer::new();
        assert_eq!(p.margin, us(150));
        // Punctual wake-ups keep the margin small.
        p.margin = p.next_margin(us(40));
        assert_eq!(p.margin, us(200));
        // One late wake-up is covered from the next wait on.
        p.margin = p.next_margin(us(4000));
        assert_eq!(p.margin, us(5150));
        // It comes back down gradually, and never below the minimum.
        let before = p.margin;
        p.margin = p.next_margin(us(40));
        assert!(p.margin < before && p.margin > us(4900));
        for _ in 0..1000 {
            p.margin = p.next_margin(Duration::ZERO);
        }
        assert!(p.margin < us(160), "{:?}", p.margin);
        // A stalled process does not turn into seconds of spinning.
        assert_eq!(p.next_margin(Duration::from_secs(3)), Pacer::MAX_MARGIN);

        let deadline = std::time::Instant::now() + Duration::from_millis(5);
        p.sleep_until(deadline);
        assert!(std::time::Instant::now() >= deadline);
    }

    #[test]
    fn rng_varies() {
        let mut r = Rng::new(0);
        let a = r.next_u32();
        assert!((0..8).map(|_| r.next_u32()).any(|v| v != a));
    }
}
