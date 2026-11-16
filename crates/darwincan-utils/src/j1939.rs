// SPDX-License-Identifier: MIT OR Apache-2.0

//! What the J1939 tools share: the two ways can-utils writes a J1939
//! address on the command line, and the lines the tools print. Written
//! from the tools' usage texts and from their output, not from their
//! source.

use darwincan::j1939::{abort, NO_ADDR, NO_NAME, NO_PGN, PGN_ADDRESS_CLAIMED, PGN_MAX};
use darwincan::{J1939Addr, J1939Filter, J1939Message, J1939Notice};

use crate::local_time;

/// An address from the command line: an interface, if one was named, and
/// what is to be bound, connected or sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub ifname: Option<String>,
    pub addr: J1939Addr,
}

impl Spec {
    pub fn none() -> Spec {
        Spec {
            ifname: None,
            addr: J1939Addr::NONE,
        }
    }
}

fn c_number(s: &str) -> Result<u64, String> {
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => s.parse(),
    };
    parsed.map_err(|_| format!("'{s}' is not a number"))
}

fn pgn(value: u64, text: &str) -> Result<u32, String> {
    if value > u64::from(PGN_MAX) {
        return Err(format!("'{text}' is not a parameter group number"));
    }
    Ok(value as u32)
}

/// Parse an address as `testj1939` and `j1939cat` take it:
/// `[IFACE][:[SA][,[PGN][,NAME]]]`, with numbers as C writes them
/// (`0x80`, `128`). `-` is no address at all.
pub fn parse_spec(s: &str) -> Result<Spec, String> {
    let mut spec = Spec::none();
    if s == "-" {
        return Ok(spec);
    }
    let (ifname, rest) = match s.split_once(':') {
        Some((ifname, rest)) => (ifname, rest),
        None => (s, ""),
    };
    if !ifname.is_empty() {
        spec.ifname = Some(ifname.to_string());
    }
    let mut parts = rest.splitn(3, ',');
    if let Some(sa) = parts.next().filter(|p| !p.is_empty()) {
        match c_number(sa)? {
            v if v <= 0xFF => spec.addr.addr = v as u8,
            _ => return Err(format!("'{sa}' is not an address")),
        }
    }
    if let Some(p) = parts.next().filter(|p| !p.is_empty()) {
        spec.addr.pgn = pgn(c_number(p)?, p)?;
    }
    if let Some(name) = parts.next().filter(|p| !p.is_empty()) {
        spec.addr.name = c_number(name)?;
    }
    Ok(spec)
}

/// Parse an address as `j1939spy` and `j1939sr` take it:
/// `[IFACE:][NAME|SA][,PGN]`, in hex without a prefix. A number of up to
/// two digits is an address, a longer one a NAME.
pub fn parse_hex_spec(s: &str) -> Result<Spec, String> {
    let mut spec = Spec::none();
    let (ifname, rest) = match s.split_once(':') {
        Some((ifname, rest)) => (Some(ifname), rest),
        // Without a colon, something that is not a number is an interface.
        None if s.split(',').next().is_some_and(|w| hex(w).is_err()) => (Some(s), ""),
        None => (None, s),
    };
    if let Some(ifname) = ifname.filter(|i| !i.is_empty()) {
        spec.ifname = Some(ifname.to_string());
    }
    let (who, group) = match rest.split_once(',') {
        Some((who, group)) => (who, group),
        None => (rest, ""),
    };
    if !who.is_empty() {
        let digits = who.trim_start_matches("0x").len();
        match hex(who)? {
            v if digits <= 2 => spec.addr.addr = v as u8,
            v => spec.addr.name = v,
        }
    }
    if !group.is_empty() {
        spec.addr.pgn = pgn(hex(group)?, group)?;
    }
    Ok(spec)
}

fn hex(s: &str) -> Result<u64, String> {
    let digits = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(digits, 16).map_err(|_| format!("'{s}' is not a hex number"))
}

/// The filter that passes what `addr` names as a sender: its NAME or
/// address, and its parameter group. `None` if it names nothing.
pub fn source_filter(addr: &J1939Addr) -> Option<J1939Filter> {
    let mut filter = J1939Filter::default();
    if addr.name != NO_NAME {
        filter.name = addr.name;
        filter.name_mask = u64::MAX;
    } else if addr.addr != NO_ADDR {
        filter.addr = addr.addr;
        filter.addr_mask = 0xFF;
    }
    if addr.pgn != NO_PGN {
        filter.pgn = addr.pgn;
        filter.pgn_mask = PGN_MAX;
    }
    (filter != J1939Filter::default()).then_some(filter)
}

/// How `j1939spy -t` shows time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpyTime {
    None,
    /// Seconds since the epoch.
    Absolute,
    /// Since the message before; the first one absolute.
    Delta,
    /// Since the first message.
    Zero,
    /// Local date and time.
    AbsoluteDate,
}

impl SpyTime {
    pub fn from_letter(letter: &str) -> Option<SpyTime> {
        Some(match letter {
            "a" => SpyTime::Absolute,
            "d" => SpyTime::Delta,
            "z" | "" => SpyTime::Zero,
            "A" => SpyTime::AbsoluteDate,
            _ => return None,
        })
    }
}

/// The time column of `j1939spy`, which keeps what it needs from one
/// message to the next.
pub struct SpyClock {
    mode: SpyTime,
    first: Option<u64>,
    last: u64,
}

impl SpyClock {
    pub fn new(mode: SpyTime) -> SpyClock {
        SpyClock {
            mode,
            first: None,
            last: 0,
        }
    }

    /// The text for a message stamped `ns`: empty without `-t`.
    pub fn stamp(&mut self, ns: u64) -> String {
        let first = *self.first.get_or_insert(ns);
        let since = |from: u64| {
            let d = ns.saturating_sub(from) / 100_000;
            format!("({}.{:04})", d / 10_000, d % 10_000)
        };
        let text = match self.mode {
            SpyTime::None => String::new(),
            SpyTime::Absolute => since(0),
            SpyTime::Zero => since(first),
            SpyTime::Delta if self.last == 0 => since(0),
            SpyTime::Delta => since(self.last),
            SpyTime::AbsoluteDate => {
                let t = local_time(ns / 1_000_000_000);
                format!(
                    "({:04}{:02}{:02}T{:02}{:02}{:02}.{:04})",
                    t.year,
                    t.month,
                    t.day,
                    t.hour,
                    t.minute,
                    t.second,
                    ns % 1_000_000_000 / 100_000
                )
            }
        };
        self.last = ns;
        text
    }
}

/// One line of `j1939spy`: the time, who sent it and which parameter
/// group, whom to, the priority, the length and the first `block` bytes.
pub fn spy_line(time: &str, ifname: &str, m: &J1939Message, block: usize) -> String {
    let source = if m.src.name == NO_NAME {
        format!("{:02x}", m.src.addr)
    } else if m.src.pgn == PGN_ADDRESS_CLAIMED {
        // A claim: the NAME, and the address it asks for.
        format!("{:016x}.{:02x}", m.src.name, m.src.addr)
    } else {
        format!("{:016x}", m.src.name)
    };
    let destination = if m.dst_name != NO_NAME {
        format!("{:016x}", m.dst_name)
    } else if m.dst_addr == NO_ADDR {
        "-".to_string()
    } else {
        format!("{:02x}", m.dst_addr)
    };
    // A message put together from several frames comes without its
    // interface, as it does on Linux.
    let interface = if m.data.len() > 8 {
        String::new()
    } else {
        format!("{ifname}:")
    };
    let shown = &m.data[..m.data.len().min(block)];
    let cut = if shown.len() < m.data.len() {
        "..."
    } else {
        ""
    };
    let mut line = format!(
        "{time} {interface}{source},{:05x} {destination} !{} [{}{cut}] ",
        m.src.pgn,
        m.priority,
        shown.len()
    );
    for (i, byte) in shown.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            line.push(' ');
        }
        line.push_str(&format!("{byte:02x}"));
    }
    line
}

/// What `testj1939 -r` prints for a message: who sent it and which
/// parameter group, then the data, eight bytes to a line.
pub fn testj1939_dump(m: &J1939Message, names: bool) -> String {
    let mut out = String::new();
    if names {
        out.push_str(&format!("{:016x} ", m.src.name));
    }
    out.push_str(&format!("{:02x} {:05x}:", m.src.addr, m.src.pgn));
    for (i, byte) in m.data.iter().enumerate() {
        if i > 0 && i % 8 == 0 {
            out.push_str(&format!("\n{i:05x}    "));
        }
        out.push_str(&format!(" {byte:02x}"));
    }
    out.push('\n');
    out
}

/// The bytes `testj1939 -s` sends: 01 23 45 67 89 ab cd ef, over and over.
pub fn testj1939_data(len: usize) -> Vec<u8> {
    const PATTERN: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];
    (0..len).map(|i| PATTERN[i % 8]).collect()
}

/// The first line `j1939cat` prints for a notice: what happened (`TX ACK`,
/// `RX RTS`, ...), when, which of its messages, and how many bytes so far.
pub fn cat_event(what: &str, timestamp_ns: u64, seq: u32, bytes: u32) -> String {
    format!(
        "  {what}: {} s {} us (seq={seq:03}, send={bytes:07})",
        timestamp_ns / 1_000_000_000,
        timestamp_ns % 1_000_000_000 / 1_000
    )
}

/// The second line `j1939cat` prints when a long message begins to arrive.
pub fn cat_rts_details(n: &J1939Notice) -> String {
    format!(
        "  total size: {}, pgn=0x{:05x}, sa=0x{:02x}, da=0x{:02x} src_name=0x{:08x}, dst_name=0x{:08x})",
        n.size, n.pgn, n.src_addr, n.dst_addr, n.src_name, n.dst_name
    )
}

/// The error Linux reports for a transport session that ended with this
/// abort reason, as its J1939 documentation lists them: the number and
/// its text. `j1939cat` prints both and exits with the number.
pub fn abort_errno(reason: u8) -> (u8, &'static str) {
    match reason {
        abort::BUSY => (114, "Operation already in progress"),
        abort::RESOURCES => (12, "Cannot allocate memory"),
        abort::TIMEOUT => (113, "No route to host"),
        abort::CTS_WHILE_SENDING => (74, "Bad message"),
        abort::RETRANSMIT_LIMIT => (131, "State not recoverable"),
        abort::UNEXPECTED_DATA => (107, "Transport endpoint is not connected"),
        abort::BAD_SEQUENCE => (84, "Invalid or incomplete multibyte or wide character"),
        abort::DUPLICATE_SEQUENCE => (71, "Protocol error"),
        abort::TOO_BIG => (75, "Value too large for defined data type"),
        _ => (71, "Protocol error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darwincan::J1939NoticeKind;

    fn message(sa: u8, pgn: u32, da: u8, priority: u8, data: &[u8]) -> J1939Message {
        J1939Message {
            timestamp_ns: 0,
            src: J1939Addr::new(sa, pgn),
            dst_addr: da,
            dst_name: NO_NAME,
            priority,
            data: data.to_vec(),
        }
    }

    #[test]
    fn addresses_as_testj1939_and_j1939cat_write_them() {
        let s = parse_spec("vcan0:0x80").unwrap();
        assert_eq!(s.ifname.as_deref(), Some("vcan0"));
        assert_eq!(s.addr, J1939Addr::address(0x80));
        let s = parse_spec(":0x90,0x12300").unwrap();
        assert_eq!((s.ifname, s.addr), (None, J1939Addr::new(0x90, 0x12300)));
        let s = parse_spec("can0:128,,0x1122334455667788").unwrap();
        assert_eq!(s.addr.addr, 128);
        assert_eq!(s.addr.pgn, NO_PGN);
        assert_eq!(s.addr.name, 0x1122_3344_5566_7788);
        assert_eq!(parse_spec("vcan0").unwrap().addr, J1939Addr::NONE);
        assert_eq!(parse_spec("-").unwrap(), Spec::none());
        assert_eq!(
            parse_spec(":0xff,0xFEB0").unwrap().addr,
            J1939Addr::broadcast(0xFEB0)
        );
        assert!(parse_spec("vcan0:0x100").is_err());
        assert!(parse_spec("vcan0:1,0x40000").is_err());
        assert!(parse_spec("vcan0:zz").is_err());
    }

    #[test]
    fn addresses_as_j1939spy_and_j1939sr_write_them() {
        let s = parse_hex_spec("vcan0:80,FEF1").unwrap();
        assert_eq!(s.ifname.as_deref(), Some("vcan0"));
        assert_eq!(s.addr, J1939Addr::new(0x80, 0xFEF1));
        let s = parse_hex_spec("vcan0:1122334455667788").unwrap();
        assert_eq!(s.addr.name, 0x1122_3344_5566_7788);
        assert_eq!(s.addr.addr, NO_ADDR);
        assert_eq!(
            parse_hex_spec("vcan0").unwrap().ifname.as_deref(),
            Some("vcan0")
        );
        assert_eq!(
            parse_hex_spec("vcan0:,FEF1").unwrap().addr,
            J1939Addr::broadcast(0xFEF1)
        );
        assert_eq!(parse_hex_spec("90").unwrap().addr, J1939Addr::address(0x90));
        assert_eq!(
            parse_hex_spec("vcan0:90,EF00").unwrap().addr,
            J1939Addr::new(0x90, 0xEF00)
        );
        assert!(parse_hex_spec("vcan0:80,zz").is_err());
    }

    #[test]
    fn a_source_filter_names_what_was_given() {
        assert_eq!(source_filter(&J1939Addr::NONE), None);
        let f = source_filter(&J1939Addr::new(0x80, 0xFEF1)).unwrap();
        assert!(f.matches(0, 0xFEF1, 0x80));
        assert!(!f.matches(0, 0xFEF2, 0x80) && !f.matches(0, 0xFEF1, 0x81));
        let by_name = J1939Addr {
            name: 7,
            ..J1939Addr::NONE
        };
        let f = source_filter(&by_name).unwrap();
        assert!(f.matches(7, 1, 2) && !f.matches(8, 1, 2));
        let f = source_filter(&J1939Addr::broadcast(0xFEF1)).unwrap();
        assert!(f.matches(9, 0xFEF1, 9) && !f.matches(9, 0xFEF2, 9));
    }

    // The lines below are what can-utils 2023.03 printed for the same
    // messages on Linux 6.8.
    #[test]
    fn spy_lines_match_can_utils() {
        let m = message(0x80, 0xFEF1, NO_ADDR, 6, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            spy_line("", "vcan9", &m, 1024),
            " vcan9:80,0fef1 - !6 [8] 01020304 05060708"
        );
        let m = message(0x80, 0xEA00, 0x90, 3, &[0xDA, 0xFE, 0x00]);
        assert_eq!(
            spy_line("", "vcan9", &m, 1024),
            " vcan9:80,0ea00 90 !3 [3] dafe00"
        );
        // A claim shows the NAME and the address; later messages the NAME.
        let mut m = message(
            0x80,
            0xEE00,
            NO_ADDR,
            6,
            &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
        );
        m.src.name = 0x1122_3344_5566_7788;
        assert_eq!(
            spy_line("", "vcan9", &m, 1024),
            " vcan9:1122334455667788.80,0ee00 - !6 [8] 88776655 44332211"
        );
        let mut m = message(0x80, 0xEF00, 0x90, 6, &[0xAA, 0xBB]);
        m.src.name = 0x1122_3344_5566_7788;
        m.dst_name = 0x8000_0000_0000_0001;
        assert_eq!(
            spy_line("", "vcan9", &m, 1024),
            " vcan9:1122334455667788,0ef00 8000000000000001 !6 [2] aabb"
        );
        // A message of several frames comes without the interface.
        let m = message(
            0x33,
            0xFEB0,
            NO_ADDR,
            7,
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0x00],
        );
        assert_eq!(
            spy_line("", "vcan9", &m, 1024),
            " 33,0feb0 - !7 [10] 11223344 55667788 9900"
        );
        // Cut to the block size.
        let m = message(0x40, 0xFEB0, NO_ADDR, 6, &testj1939_data(20));
        assert_eq!(
            spy_line("", "vcan9", &m, 8),
            " 40,0feb0 - !6 [8...] 01234567 89abcdef"
        );
        assert_eq!(
            spy_line(
                "(0.1077)",
                "vcan9",
                &message(0x80, 0xFEF1, NO_ADDR, 6, &[1]),
                1024
            ),
            "(0.1077) vcan9:80,0fef1 - !6 [1] 01"
        );
    }

    #[test]
    fn spy_times_come_in_four_kinds() {
        let (a, b) = (1_794_848_906_197_012_345, 1_794_848_906_299_812_345);
        let mut c = SpyClock::new(SpyTime::None);
        assert_eq!(c.stamp(a), "");
        let mut c = SpyClock::new(SpyTime::Absolute);
        assert_eq!(
            (c.stamp(a), c.stamp(b)),
            ("(1794848906.1970)".into(), "(1794848906.2998)".into())
        );
        let mut c = SpyClock::new(SpyTime::Zero);
        assert_eq!(
            (c.stamp(a), c.stamp(b)),
            ("(0.0000)".into(), "(0.1028)".into())
        );
        let mut c = SpyClock::new(SpyTime::Delta);
        assert_eq!(
            (c.stamp(a), c.stamp(b), c.stamp(b)),
            (
                "(1794848906.1970)".into(),
                "(0.1028)".into(),
                "(0.0000)".into()
            )
        );
        let mut c = SpyClock::new(SpyTime::AbsoluteDate);
        let text = c.stamp(a);
        assert_eq!(text.len(), "(20261116T090836.7201)".len());
        assert!(text.ends_with(".1970)") && &text[9..10] == "T");
        assert_eq!(SpyTime::from_letter(""), Some(SpyTime::Zero));
        assert_eq!(SpyTime::from_letter("x"), None);
    }

    #[test]
    fn testj1939_output_matches_can_utils() {
        let m = message(0x80, 0xEF00, 0x90, 6, &[1, 2, 3, 4, 5]);
        assert_eq!(testj1939_dump(&m, false), "80 0ef00: 01 02 03 04 05\n");
        let m = message(0x80, 0xEF00, 0x90, 6, &testj1939_data(20));
        assert_eq!(
            testj1939_dump(&m, false),
            "80 0ef00: 01 23 45 67 89 ab cd ef\n\
             00008     01 23 45 67 89 ab cd ef\n\
             00010     01 23 45 67\n"
        );
        let mut m = message(0x80, 0xEF00, 0x90, 6, &[1, 2]);
        m.src.name = 0x1122_3344_5566_7788;
        assert_eq!(
            testj1939_dump(&m, true),
            "1122334455667788 80 0ef00: 01 02\n"
        );
    }

    #[test]
    fn j1939cat_lines_match_can_utils() {
        assert_eq!(
            cat_event("TX ACK", 1_794_850_041_678_580_000, 0, 20),
            "  TX ACK: 1794850041 s 678580 us (seq=000, send=0000020)"
        );
        assert_eq!(
            cat_event("TX ENQ", 1_794_850_045_854_259_000, 1, 3),
            "  TX ENQ: 1794850045 s 854259 us (seq=001, send=0000003)"
        );
        let n = J1939Notice {
            kind: J1939NoticeKind::RxStarted,
            timestamp_ns: 0,
            pgn: 0x12300,
            src_addr: 0x80,
            dst_addr: 0x90,
            src_name: 0,
            dst_name: 0,
            size: 20,
            acked: 0,
            reason: 0,
        };
        assert_eq!(
            cat_rts_details(&n),
            "  total size: 20, pgn=0x12300, sa=0x80, da=0x90 src_name=0x00000000, dst_name=0x00000000)"
        );
        assert_eq!(abort_errno(abort::TIMEOUT), (113, "No route to host"));
        assert_eq!(
            abort_errno(abort::RETRANSMIT_LIMIT),
            (131, "State not recoverable")
        );
    }
}
