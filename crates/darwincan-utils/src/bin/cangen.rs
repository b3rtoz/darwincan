// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use darwincan::{
    Frame, RawSocket, CANFD_BRS, CANFD_ESI, CANFD_FDF, CAN_EFF_FLAG, CAN_EFF_MASK, CAN_RTR_FLAG,
    CAN_SFF_MASK,
};
use darwincan_utils::{
    check_sent, dotted_frame, fail, getopt, long_frame, prefer_precise_timing, usage, wants_help,
    Pacer, Rng,
};

const TOOL: &str = "cangen";
const USAGE: &str = "\
Usage: cangen [options] <interface>

Generate CAN frames for testing.

Options:
  -g <ms>       Gap between frames in milliseconds, may be fractional
                (default: 200)
  -c <count>    Frames to send in a burst before each gap (default: 1)
  -n <count>    Stop after this many frames (default: run until interrupted)
  -e            Extended (29-bit) identifiers
  -f            CAN FD frames
  -b            CAN FD frames with bit rate switch
  -E            CAN FD frames with the error state indicator set
  -R            Remote frames
  -m            Mix classic, extended, remote and CAN FD frames at random
  -I <mode>     Identifier: r, e, o, i or a fixed hex value
  -L <mode>     Length: r, i or a fixed decimal value
  -D <mode>     Data: r, i or fixed hex bytes; a nibble written as 'x' is
                random
  -x            Do not deliver the frames to other local programs
  -v            Print each frame sent; -v -v in candump layout, -v -v -v
                with ASCII
  -i, -p <ms>   Accepted for compatibility: sends never fail for lack of
                buffer space, they wait for the daemon instead

Modes: r random (default), e random even, o random odd (identifiers only),
i incrementing. Incrementing data uses a length of at least 1.

Examples:
  cangen vcan0
  cangen vcan0 -g 4 -I 42A -L 1 -D i -v -v
  cangen vcan0 -e -L i -v -v -v
  cangen vcan0 -D 11xx3344DEADBEEF -L 8
  cangen vcan0 -g 0 -x -n 100000";

/// can-utils options that have no counterpart here.
const UNSUPPORTED: &str = "atX8F:S:A:V:P:";

/// Payload lengths a CAN FD data length code can express.
const FD_LENGTHS: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdMode {
    Random,
    Even,
    Odd,
    Increment,
    Fixed(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LenMode {
    Random,
    Increment,
    Fixed(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DataMode {
    Random,
    Increment,
    /// Each byte with a mask of its bits to randomise.
    Fixed(Vec<(u8, u8)>),
}

fn parse_id_mode(s: &str) -> Result<IdMode, String> {
    match s.chars().next() {
        Some('r') => Ok(IdMode::Random),
        Some('e') => Ok(IdMode::Even),
        Some('o') => Ok(IdMode::Odd),
        Some('i') => Ok(IdMode::Increment),
        _ => u32::from_str_radix(s, 16)
            .map(IdMode::Fixed)
            .map_err(|_| format!("invalid identifier mode: {s}")),
    }
}

fn parse_len_mode(s: &str) -> Result<LenMode, String> {
    match s.chars().next() {
        Some('r') => Ok(LenMode::Random),
        Some('i') => Ok(LenMode::Increment),
        _ => s
            .parse::<usize>()
            .ok()
            .filter(|n| *n <= 64)
            .map(LenMode::Fixed)
            .ok_or(format!("invalid length mode: {s}")),
    }
}

fn parse_data_mode(s: &str) -> Result<DataMode, String> {
    match s {
        "r" => return Ok(DataMode::Random),
        "i" => return Ok(DataMode::Increment),
        _ => {}
    }
    let bad = || format!("invalid data mode: {s}");
    let nibbles: Vec<char> = s.chars().collect();
    if nibbles.is_empty() || nibbles.len() % 2 != 0 || nibbles.len() > 128 {
        return Err(bad());
    }
    nibbles
        .chunks(2)
        .map(|pair| {
            let mut value = 0u8;
            let mut random = 0u8;
            for (&c, shift) in pair.iter().zip([4, 0]) {
                if c == 'x' {
                    random |= 0xF << shift;
                } else {
                    value |= (c.to_digit(16).ok_or_else(bad)? as u8) << shift;
                }
            }
            Ok((value, random))
        })
        .collect::<Result<_, _>>()
        .map(DataMode::Fixed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    extended: bool,
    fd: bool,
    brs: bool,
    esi: bool,
    remote: bool,
    mix: bool,
    id: IdMode,
    len: LenMode,
    data: DataMode,
}

/// Produces the frame sequence.
struct Generator {
    config: Config,
    rng: Rng,
    next_id: u32,
    next_dlc: usize,
    counter: u64,
}

impl Generator {
    fn new(config: Config, rng: Rng) -> Generator {
        Generator {
            config,
            rng,
            next_id: 0,
            next_dlc: 0,
            counter: 0,
        }
    }

    fn next(&mut self) -> Frame {
        let c = &self.config;
        let (mut extended, mut fd, mut brs, mut esi, mut remote) =
            (c.extended, c.fd, c.brs, c.esi, c.remote);
        if c.mix {
            let r = self.rng.next_u32();
            extended = r & 1 != 0;
            fd = r & 2 != 0;
            brs = fd && r & 4 != 0;
            esi = fd && r & 8 != 0;
            // One classic frame in four is a remote request.
            remote = !fd && r & 0x30 == 0x30;
        }
        let remote = remote && !fd;

        let mask = if extended { CAN_EFF_MASK } else { CAN_SFF_MASK };
        let id = match c.id {
            IdMode::Random => self.rng.next_u32(),
            IdMode::Even => self.rng.next_u32() & !1,
            IdMode::Odd => self.rng.next_u32() | 1,
            IdMode::Fixed(v) => v,
            IdMode::Increment => {
                let v = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                v
            }
        } & mask;

        let mut len = match c.len {
            LenMode::Random if fd => FD_LENGTHS[(self.rng.next_u32() & 0xF) as usize],
            // Half the classic frames are full length.
            LenMode::Random => ((self.rng.next_u32() & 0xF) as usize).min(8),
            LenMode::Fixed(n) if fd => *FD_LENGTHS.iter().find(|l| **l >= n).unwrap_or(&64),
            LenMode::Fixed(n) => n.min(8),
            LenMode::Increment => {
                let dlc = self.next_dlc;
                self.next_dlc += 1;
                if fd && !c.mix {
                    FD_LENGTHS[dlc % 16]
                } else {
                    dlc % 9
                }
            }
        };
        if c.data == DataMode::Increment && len == 0 {
            len = 1;
        }

        let mut data = [0u8; 64];
        match &c.data {
            DataMode::Random => {
                let random = self.rng.next_u64().to_le_bytes();
                // Longer CAN FD payloads repeat the same eight bytes.
                for chunk in data.chunks_mut(8) {
                    chunk.copy_from_slice(&random);
                }
            }
            DataMode::Increment => {
                data[..8].copy_from_slice(&self.counter.to_le_bytes());
                self.counter = self.counter.wrapping_add(1);
            }
            DataMode::Fixed(bytes) => {
                for (out, (value, random)) in data.iter_mut().zip(bytes) {
                    *out = *value;
                    if *random != 0 {
                        *out |= self.rng.next_u32() as u8 & random;
                    }
                }
            }
        }

        let mut can_id = id;
        if extended {
            can_id |= CAN_EFF_FLAG;
        }
        if remote {
            can_id |= CAN_RTR_FLAG;
        }
        let mut flags = 0;
        if fd {
            flags = CANFD_FDF;
            if brs {
                flags |= CANFD_BRS;
            }
            if esi {
                flags |= CANFD_ESI;
            }
        }
        Frame::from_raw(can_id, flags, &data[..len]).expect("generated frames are valid")
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let opts = match getopt(args, &format!("g:c:n:efbERmI:L:D:xvip:{UNSUPPORTED}")) {
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
    let [ifname] = opts.positional.as_slice() else {
        return usage(TOOL, "expected one interface", USAGE);
    };

    let parsed = (|| -> Result<_, String> {
        let gap_ms = match opts.value('g') {
            None => 200.0,
            Some(v) => v
                .parse::<f64>()
                .ok()
                .filter(|g| g.is_finite() && *g >= 0.0)
                .ok_or(format!("invalid gap: {v}"))?,
        };
        let count = |flag: char, what: &str| match opts.value(flag) {
            None => Ok(None),
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or(format!("invalid {what}: {v}")),
        };
        let config = Config {
            extended: opts.has('e'),
            fd: opts.has('f') || opts.has('b') || opts.has('E'),
            brs: opts.has('b'),
            esi: opts.has('E'),
            remote: opts.has('R'),
            mix: opts.has('m'),
            id: opts.value('I').map_or(Ok(IdMode::Random), parse_id_mode)?,
            len: opts
                .value('L')
                .map_or(Ok(LenMode::Random), parse_len_mode)?,
            data: opts
                .value('D')
                .map_or(Ok(DataMode::Random), parse_data_mode)?,
        };
        Ok((
            Duration::from_secs_f64(gap_ms / 1000.0),
            count('c', "burst count")?.unwrap_or(1),
            count('n', "frame count")?,
            config,
        ))
    })();
    let (gap, burst, mut remaining, config) = match parsed {
        Ok(p) => p,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let verbose = opts.count('v');

    let sock = match RawSocket::open(ifname) {
        Ok(s) => s,
        Err(e) => return fail(TOOL, e),
    };
    // This socket only sends; what comes back are transmit failures.
    let setup = sock.set_filters(&[]).and_then(|()| {
        if opts.has('x') {
            sock.set_loopback(false)
        } else {
            Ok(())
        }
    });
    if let Err(e) = setup {
        return fail(TOOL, e);
    }

    // Paced sending wants punctual wake-ups; at full speed the thread never
    // sleeps and must stay an ordinary one.
    if !gap.is_zero() {
        prefer_precise_timing();
    }
    let mut pacer = Pacer::calibrated();
    let mut generator = Generator::new(config, Rng::from_clock());
    let stdout = io::stdout();
    let mut deadline = Instant::now();
    let mut unchecked = 0u32;
    loop {
        for _ in 0..burst {
            let frame = generator.next();
            if let Err(e) = sock.send(&frame) {
                return fail(TOOL, e);
            }
            if verbose > 0 {
                let text = if verbose > 1 {
                    long_frame(&frame, false, verbose > 2)
                } else {
                    dotted_frame(&frame)
                };
                if writeln!(stdout.lock(), "  {ifname}  {text}").is_err() {
                    return ExitCode::SUCCESS;
                }
            }
            if let Some(n) = &mut remaining {
                *n -= 1;
                if *n == 0 {
                    return match check_sent(&sock) {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => fail(TOOL, e),
                    };
                }
            }
            unchecked += 1;
        }
        // Check for failures, and keep pace with the daemon, whenever
        // there is time for it: every gap, or every so often at full speed.
        if !gap.is_zero() || unchecked >= 256 {
            unchecked = 0;
            if let Err(e) = check_sent(&sock) {
                return fail(TOOL, e);
            }
        }
        if !gap.is_zero() {
            deadline += gap;
            let now = Instant::now();
            if deadline < now {
                // Fell behind (suspended, or a gap shorter than a send).
                deadline = now;
            }
            pacer.sleep_until(deadline);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            extended: false,
            fd: false,
            brs: false,
            esi: false,
            remote: false,
            mix: false,
            id: IdMode::Random,
            len: LenMode::Random,
            data: DataMode::Random,
        }
    }

    fn frames(config: Config, n: usize) -> Vec<Frame> {
        let mut g = Generator::new(config, Rng::new(42));
        (0..n).map(|_| g.next()).collect()
    }

    #[test]
    fn modes_parse() {
        assert_eq!(parse_id_mode("r"), Ok(IdMode::Random));
        assert_eq!(parse_id_mode("i"), Ok(IdMode::Increment));
        assert_eq!(parse_id_mode("e"), Ok(IdMode::Even));
        assert_eq!(parse_id_mode("o"), Ok(IdMode::Odd));
        assert_eq!(parse_id_mode("42A"), Ok(IdMode::Fixed(0x42A)));
        assert!(parse_id_mode("xyz").is_err());
        assert_eq!(parse_len_mode("i"), Ok(LenMode::Increment));
        assert_eq!(parse_len_mode("8"), Ok(LenMode::Fixed(8)));
        assert!(parse_len_mode("65").is_err());
        assert_eq!(parse_data_mode("i"), Ok(DataMode::Increment));
        assert_eq!(
            parse_data_mode("11xx3x"),
            Ok(DataMode::Fixed(vec![(0x11, 0), (0, 0xFF), (0x30, 0x0F)]))
        );
        assert!(parse_data_mode("123").is_err());
        assert!(parse_data_mode("zz").is_err());
    }

    #[test]
    fn default_frames_are_random_classic_frames() {
        let all = frames(config(), 500);
        assert!(all
            .iter()
            .all(|f| !f.is_extended() && !f.is_fd() && !f.is_remote() && f.len() <= 8));
        let ids: std::collections::HashSet<u32> = all.iter().map(|f| f.id().raw()).collect();
        assert!(ids.len() > 100);
        // Every length occurs, and full-length frames are about half.
        for len in 0..=8 {
            assert!(all.iter().any(|f| f.len() == len), "length {len}");
        }
        let full = all.iter().filter(|f| f.len() == 8).count();
        assert!((150..350).contains(&full), "{full}");
    }

    #[test]
    fn fixed_id_and_length_with_incrementing_data() {
        // cangen vcan0 -I 42A -L 1 -D i
        let all = frames(
            Config {
                id: IdMode::Fixed(0x42A),
                len: LenMode::Fixed(1),
                data: DataMode::Increment,
                ..config()
            },
            3,
        );
        let text: Vec<String> = all.iter().map(Frame::to_string).collect();
        assert_eq!(text, ["42A#00", "42A#01", "42A#02"]);
    }

    #[test]
    fn incrementing_id_and_length() {
        let all = frames(
            Config {
                extended: true,
                id: IdMode::Increment,
                len: LenMode::Increment,
                data: DataMode::Fixed(vec![(0xAA, 0); 8]),
                ..config()
            },
            11,
        );
        let ids: Vec<u32> = all.iter().map(|f| f.id().raw()).collect();
        assert_eq!(ids, (0..11).collect::<Vec<u32>>());
        assert!(all.iter().all(Frame::is_extended));
        let lens: Vec<usize> = all.iter().map(Frame::len).collect();
        assert_eq!(lens, [0, 1, 2, 3, 4, 5, 6, 7, 8, 0, 1]);
        assert_eq!(all[2].data(), &[0xAA, 0xAA]);

        // Incrementing data never goes out empty.
        let all = frames(
            Config {
                len: LenMode::Increment,
                data: DataMode::Increment,
                ..config()
            },
            2,
        );
        assert_eq!(all[0].len(), 1);
    }

    #[test]
    fn can_fd_lengths_and_flags() {
        let all = frames(
            Config {
                fd: true,
                brs: true,
                len: LenMode::Increment,
                ..config()
            },
            16,
        );
        let lens: Vec<usize> = all.iter().map(Frame::len).collect();
        assert_eq!(lens, FD_LENGTHS);
        assert!(all.iter().all(|f| f.is_fd() && f.is_brs()));
        // The eight random bytes repeat through a long payload.
        let long = all.last().unwrap().data();
        assert_eq!(long[..8], long[56..]);

        let all = frames(
            Config {
                fd: true,
                esi: true,
                len: LenMode::Fixed(9),
                ..config()
            },
            1,
        );
        assert_eq!(all[0].len(), 12);
        assert_eq!(all[0].flags(), CANFD_FDF | CANFD_ESI);
    }

    #[test]
    fn id_parities_remote_and_masks() {
        let even = frames(
            Config {
                id: IdMode::Even,
                ..config()
            },
            50,
        );
        assert!(even.iter().all(|f| f.id().raw() % 2 == 0));
        let odd = frames(
            Config {
                id: IdMode::Odd,
                ..config()
            },
            50,
        );
        assert!(odd.iter().all(|f| f.id().raw() % 2 == 1));

        // A fixed identifier too wide for 11 bits is cut down, as in can-utils.
        let cut = frames(
            Config {
                id: IdMode::Fixed(0x1234_5678),
                ..config()
            },
            1,
        );
        assert_eq!(cut[0].id().raw(), 0x678);

        let remote = frames(
            Config {
                remote: true,
                len: LenMode::Fixed(4),
                ..config()
            },
            1,
        );
        assert_eq!(remote[0].to_string()[3..], *"#R4");
    }

    #[test]
    fn random_nibbles_and_mixing() {
        let all = frames(
            Config {
                len: LenMode::Fixed(3),
                data: DataMode::Fixed(vec![(0x11, 0), (0, 0xFF), (0x30, 0x0F)]),
                ..config()
            },
            64,
        );
        assert!(all
            .iter()
            .all(|f| f.data()[0] == 0x11 && f.data()[2] & 0xF0 == 0x30));
        let seconds: std::collections::HashSet<u8> = all.iter().map(|f| f.data()[1]).collect();
        assert!(seconds.len() > 16);

        let mixed = frames(
            Config {
                mix: true,
                ..config()
            },
            400,
        );
        for kind in [
            Frame::is_extended as fn(&Frame) -> bool,
            Frame::is_fd,
            Frame::is_brs,
            Frame::is_remote,
            |f| !f.is_extended() && !f.is_fd() && !f.is_remote(),
        ] {
            assert!(mixed.iter().any(kind));
        }
        assert!(mixed.iter().all(|f| !(f.is_fd() && f.is_remote())));
    }
}
