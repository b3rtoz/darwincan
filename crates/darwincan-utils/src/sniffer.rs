// SPDX-License-Identifier: MIT OR Apache-2.0

//! What `cansniffer` keeps and draws: one row for each CAN identifier,
//! with its latest data, the time between its last two frames, and the
//! bytes that changed. No I/O and no clock: the caller passes in frames,
//! commands and the time, and writes out what it gets back. Written from
//! the usage text and the screen of can-utils' `cansniffer`, not from its
//! source.

use std::collections::BTreeMap;

use darwincan::Frame;

const CLEAR: &str = "\x1b[2J\x1b[H";
const HOME: &str = "\x1b[H";
const DOWN: &str = "\x1b[B";
const MARK: &str = "\x1b[1m\x1b[31m";
const RESET: &str = "\x1b[0m";
/// Sent once at the start, and at the end.
pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";

/// How the data is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Hex,
    /// Eight binary digits a byte, run together.
    Binary,
    /// The same in a narrower layout, for 29-bit identifiers on 80 columns.
    BinaryNarrow,
    /// With a gap after each byte.
    BinaryGap,
}

/// A slot's place: 11-bit identifiers first, then 29-bit ones.
type Key = (bool, u32);

struct Slot {
    data: Vec<u8>,
    /// The bits that changed, until the hold time is over.
    changed: Vec<u8>,
    /// Bits whose changes are passed over.
    notch: Vec<u8>,
    changed_at_ms: u64,
    last_ms: u64,
    /// Milliseconds between its last two frames.
    interval_ms: u64,
    /// Its row has to be drawn again.
    dirty: bool,
}

/// A command to add or remove identifiers: those that equal `id` under
/// `mask`, of the 11-bit or the 29-bit kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rule {
    extended: bool,
    id: u32,
    mask: u32,
    on: bool,
}

/// What a command asks of the caller.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Carry,
    Quit,
    /// Write this to the settings file of that name.
    Write(String, String),
    /// Read the settings file of that name and pass it to
    /// [`Sniffer::load`].
    Read(String),
}

pub struct Sniffer {
    ifname: String,
    slots: BTreeMap<Key, Slot>,
    rules: Vec<Rule>,
    pub view: View,
    pub colour: bool,
    pub ascii: bool,
    /// The wide identifier column: fixed with `-e`, or once a 29-bit
    /// identifier has been seen.
    pub wide: bool,
    /// CAN FD frames are shown.
    pub fd: bool,
    /// In hundredths of a second, as on the command line: how long a row
    /// stays without a new frame (0: for good), how long a change stays
    /// marked, and how often the screen is drawn.
    pub timeout: u64,
    pub hold: u64,
    pub interval: u64,
    counter: u8,
    redraw: bool,
    seen_any: bool,
}

impl Sniffer {
    pub fn new(ifname: &str) -> Sniffer {
        Sniffer {
            ifname: ifname.to_string(),
            slots: BTreeMap::new(),
            rules: Vec::new(),
            view: View::Hex,
            colour: false,
            ascii: true,
            wide: false,
            fd: true,
            timeout: 500,
            hold: 100,
            interval: 20,
            counter: 0,
            redraw: true,
            seen_any: false,
        }
    }

    /// Start with no identifier shown (`-q`).
    pub fn quiet(&mut self) {
        self.rules = vec![
            Rule {
                extended: false,
                id: 0,
                mask: 0,
                on: false,
            },
            Rule {
                extended: true,
                id: 0,
                mask: 0,
                on: false,
            },
        ];
    }

    fn enabled(&self, key: Key) -> bool {
        self.rules
            .iter()
            .rev()
            .find(|r| r.extended == key.0 && key.1 & r.mask == r.id & r.mask)
            .map_or(true, |r| r.on)
    }

    fn rule(&mut self, rule: Rule) {
        self.rules.push(rule);
        let before = self.slots.len();
        let rules = std::mem::take(&mut self.rules);
        self.slots.retain(|key, _| {
            rules
                .iter()
                .rev()
                .find(|r| r.extended == key.0 && key.1 & r.mask == r.id & r.mask)
                .map_or(true, |r| r.on)
        });
        self.rules = rules;
        self.redraw |= self.slots.len() != before;
    }

    /// A frame was received at `now_ms`.
    pub fn on_frame(&mut self, frame: &Frame, now_ms: u64) {
        if frame.is_error() || frame.is_remote() || (frame.is_fd() && !self.fd) {
            return;
        }
        let key = (frame.is_extended(), frame.id().raw());
        if !self.enabled(key) {
            return;
        }
        self.seen_any = true;
        if key.0 && !self.wide {
            self.wide = true;
            self.redraw = true;
        }
        let data = frame.data();
        match self.slots.get_mut(&key) {
            None => {
                self.slots.insert(
                    key,
                    Slot {
                        data: data.to_vec(),
                        changed: vec![0; data.len()],
                        notch: Vec::new(),
                        changed_at_ms: now_ms,
                        last_ms: now_ms,
                        interval_ms: 99_999,
                        dirty: true,
                    },
                );
                self.redraw = true;
            }
            Some(slot) => {
                slot.interval_ms = (now_ms - slot.last_ms).min(99_999);
                slot.last_ms = now_ms;
                let mut changed = vec![0u8; data.len()];
                for (i, byte) in data.iter().enumerate() {
                    let before = slot.data.get(i).copied().unwrap_or(!*byte);
                    let notch = slot.notch.get(i).copied().unwrap_or(0);
                    changed[i] = (before ^ byte) & !notch;
                }
                if changed.iter().any(|c| *c != 0) || data.len() != slot.data.len() {
                    // The marks are those of the latest change.
                    slot.changed = changed;
                    slot.changed_at_ms = now_ms;
                }
                slot.changed.resize(data.len(), 0);
                slot.data = data.to_vec();
                slot.dirty = true;
            }
        }
    }

    fn header(&self) -> String {
        let columns = match (self.view == View::BinaryNarrow, self.wide) {
            (false, false) => "XX|ms | ID  | data ...     ",
            (false, true) => "XX|ms | -- ID -- | data ...     ",
            (true, false) => "XX|ms|ID |data ...     ",
            (true, true) => "XX|ms|-- ID --|data ...     ",
        };
        format!(
            "{columns}< {} {} l={} h={} t={} slots={} >",
            self.ifname,
            if self.seen_any { '#' } else { '*' },
            self.interval,
            self.hold,
            self.timeout,
            self.slots.len()
        )
    }

    fn row(&self, key: Key, slot: &Slot) -> String {
        let id = match (self.wide, key.0) {
            (false, _) => format!("{:03X}", key.1),
            (true, false) => format!("---- {:03X}", key.1),
            (true, true) => format!("{:08X}", key.1),
        };
        let between = if self.view == View::BinaryNarrow {
            "|"
        } else {
            " | "
        };
        let mut s = format!("{:05}{between}{id}{between}", slot.interval_ms);
        for (i, byte) in slot.data.iter().enumerate() {
            let marked = self.colour && slot.changed.get(i).is_some_and(|c| *c != 0);
            let text = match self.view {
                View::Hex => format!("{byte:02X}"),
                _ => format!("{byte:08b}"),
            };
            if marked {
                s.push_str(&format!("{MARK}{text}{RESET}"));
            } else {
                s.push_str(&text);
            }
            if matches!(self.view, View::Hex | View::BinaryGap) {
                s.push(' ');
            }
        }
        if self.view == View::Hex && self.ascii && slot.data.len() <= 8 {
            s.push_str(&" ".repeat(3 * (8 - slot.data.len())));
            s.extend(slot.data.iter().map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            }));
        }
        s
    }

    /// What to write to the screen at `now_ms`: everything when rows came
    /// or went, otherwise the rows that changed.
    pub fn draw(&mut self, now_ms: u64) -> String {
        let (timeout, hold) = (self.timeout * 10, self.hold * 10);
        if timeout > 0 {
            let before = self.slots.len();
            self.slots
                .retain(|_, slot| now_ms.saturating_sub(slot.last_ms) <= timeout);
            self.redraw |= self.slots.len() != before;
        }
        for slot in self.slots.values_mut() {
            if slot.changed.iter().any(|c| *c != 0)
                && now_ms.saturating_sub(slot.changed_at_ms) > hold
            {
                slot.changed.iter_mut().for_each(|c| *c = 0);
                slot.dirty = true;
            }
        }
        let mut out = String::new();
        if self.redraw {
            out.push_str(CLEAR);
            out.push_str(&self.header());
        }
        out.push_str(&format!("{HOME}{:02X}\n", self.counter));
        self.counter = self.counter.wrapping_add(1);
        let all = self.redraw;
        let rows: Vec<Option<String>> = self
            .slots
            .iter()
            .map(|(key, slot)| (all || slot.dirty).then(|| self.row(*key, slot)))
            .collect();
        for row in rows {
            match row {
                Some(row) => {
                    out.push_str(&row);
                    out.push('\n');
                }
                None => out.push_str(DOWN),
            }
        }
        self.slots.values_mut().for_each(|slot| slot.dirty = false);
        self.redraw = false;
        out
    }

    /// The identifiers a filter names: three hex digits for one 11-bit
    /// identifier, six for an identifier and a mask, eight and sixteen
    /// for the 29-bit kind.
    fn filter(text: &str, on: bool) -> Option<Rule> {
        let number = |t: &str| u32::from_str_radix(t, 16).ok();
        let (extended, id, mask) = match text.len() {
            3 => (false, number(text)?, 0x7FF),
            6 => (false, number(&text[..3])?, number(&text[3..])?),
            8 => (true, number(text)?, 0x1FFF_FFFF),
            16 => (true, number(&text[..8])?, number(&text[8..])?),
            _ => return None,
        };
        Some(Rule {
            extended,
            id,
            mask,
            on,
        })
    }

    /// A line typed while the program runs.
    pub fn command(&mut self, line: &str) -> Reply {
        let toggle = |now: View, wanted: View| if now == wanted { View::Hex } else { wanted };
        let everything = |extended: bool, on: bool| Rule {
            extended,
            id: 0,
            mask: 0,
            on,
        };
        let mut chars = line.chars();
        match chars.next() {
            Some('q') => return Reply::Quit,
            Some('b') => self.view = toggle(self.view, View::Binary),
            Some('8') => self.view = toggle(self.view, View::BinaryNarrow),
            Some('B') => self.view = toggle(self.view, View::BinaryGap),
            Some('c') => self.colour = !self.colour,
            Some('@') => self.ascii = !self.ascii,
            Some(' ') => {}
            Some('#') => {
                for slot in self.slots.values_mut() {
                    slot.notch
                        .resize(slot.changed.len().max(slot.notch.len()), 0);
                    for (notch, changed) in slot.notch.iter_mut().zip(&slot.changed) {
                        *notch |= changed;
                    }
                }
            }
            Some('*') => self.slots.values_mut().for_each(|slot| slot.notch.clear()),
            Some('r') => return Reply::Read(chars.as_str().trim().to_string()),
            Some('w') => return Reply::Write(chars.as_str().trim().to_string(), self.settings()),
            Some('a') => self.rule(everything(false, true)),
            Some('n') => self.rule(everything(false, false)),
            Some('A') => self.rule(everything(true, true)),
            Some('N') => self.rule(everything(true, false)),
            Some(sign @ ('+' | '-')) => {
                if let Some(rule) = Sniffer::filter(chars.as_str().trim(), sign == '+') {
                    self.rule(rule);
                }
            }
            _ => return Reply::Carry,
        }
        self.redraw = true;
        Reply::Carry
    }

    /// The settings file: a line for each identifier shown, with whether
    /// it is on and which of its bits are notched.
    pub fn settings(&self) -> String {
        let mut text = String::new();
        for (key, slot) in &self.slots {
            let id = key.1 | if key.0 { 0x8000_0000 } else { 0 };
            let mut notch = [0u8; 8];
            for (to, from) in notch.iter_mut().zip(&slot.notch) {
                *to = *from;
            }
            text.push_str(&format!("<{id:08X}>1.{:016X}\n", u64::from_be_bytes(notch)));
        }
        text
    }

    /// Take the identifiers and notches of a settings file.
    pub fn load(&mut self, text: &str) {
        for line in text.lines() {
            let parsed = (|| {
                let rest = line.strip_prefix('<')?;
                let (id, rest) = rest.split_once('>')?;
                let (on, notch) = rest.split_once('.')?;
                Some((
                    u32::from_str_radix(id, 16).ok()?,
                    on == "1",
                    u64::from_str_radix(notch, 16).ok()?,
                ))
            })();
            let Some((id, on, notch)) = parsed else {
                continue;
            };
            let key = (id & 0x8000_0000 != 0, id & 0x1FFF_FFFF);
            self.rule(Rule {
                extended: key.0,
                id: key.1,
                mask: if key.0 { 0x1FFF_FFFF } else { 0x7FF },
                on,
            });
            if let Some(slot) = self.slots.get_mut(&key) {
                slot.notch = notch.to_be_bytes().to_vec();
            }
        }
        self.redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Frame {
        text.parse().unwrap()
    }

    // The screens below are what can-utils 2023.03 wrote for these frames.
    #[test]
    fn the_screen_is_drawn_as_can_utils_draws_it() {
        let mut s = Sniffer::new("vcan9");
        assert_eq!(
            s.draw(0),
            "\x1b[2J\x1b[HXX|ms | ID  | data ...     < vcan9 * l=20 h=100 t=500 slots=0 >\x1b[H00\n"
        );
        assert_eq!(s.draw(200), "\x1b[H01\n");
        s.on_frame(&frame("123#0102030405060708"), 300);
        s.on_frame(&frame("124#41424344"), 300);
        assert_eq!(
            s.draw(400),
            "\x1b[2J\x1b[HXX|ms | ID  | data ...     < vcan9 # l=20 h=100 t=500 slots=2 >\x1b[H02\n\
             99999 | 123 | 01 02 03 04 05 06 07 08 ........\n\
             99999 | 124 | 41 42 43 44             ABCD\n"
        );
        // A 29-bit identifier widens the column, for good.
        s.on_frame(&frame("123#0102030405060709"), 560);
        s.on_frame(&frame("12345678#AABB"), 560);
        assert_eq!(
            s.draw(600),
            "\x1b[2J\x1b[HXX|ms | -- ID -- | data ...     < vcan9 # l=20 h=100 t=500 slots=3 >\x1b[H03\n\
             00260 | ---- 123 | 01 02 03 04 05 06 07 09 ........\n\
             99999 | ---- 124 | 41 42 43 44             ABCD\n\
             99999 | 12345678 | AA BB                   ..\n"
        );
        s.on_frame(&frame("123#0102FF0405060709"), 819);
        s.on_frame(&frame("7FF#"), 819);
        assert_eq!(
            s.draw(820),
            "\x1b[2J\x1b[HXX|ms | -- ID -- | data ...     < vcan9 # l=20 h=100 t=500 slots=4 >\x1b[H04\n\
             00259 | ---- 123 | 01 02 FF 04 05 06 07 09 ........\n\
             99999 | ---- 124 | 41 42 43 44             ABCD\n\
             99999 | ---- 7FF |                         \n\
             99999 | 12345678 | AA BB                   ..\n"
        );
        // Nothing new: the cursor only passes over the rows.
        assert_eq!(s.draw(1000), "\x1b[H05\n\x1b[B\x1b[B\x1b[B\x1b[B");
        // One row changed: it alone is written again.
        s.on_frame(&frame("124#41424345"), 1100);
        assert_eq!(
            s.draw(1200),
            "\x1b[H06\n\x1b[B00800 | ---- 124 | 41 42 43 45             ABCE\n\x1b[B\x1b[B"
        );
    }

    /// A sniffer that has seen the four frames of the capture.
    fn seen() -> Sniffer {
        let mut s = Sniffer::new("vcan9");
        for (text, at) in [
            ("123#0102030405060709", 0),
            ("124#41424344", 0),
            ("12345678#AABB", 0),
            ("7FF#", 0),
            ("123#0102FF0405060709", 257),
        ] {
            s.on_frame(&frame(text), at);
        }
        s
    }

    fn rows(s: &mut Sniffer, now: u64) -> Vec<String> {
        s.draw(now).lines().skip(1).map(str::to_string).collect()
    }

    #[test]
    fn changes_are_marked_for_the_hold_time() {
        let mut s = seen();
        s.colour = true;
        assert_eq!(
            rows(&mut s, 300)[0],
            "00257 | ---- 123 | 01 02 \x1b[1m\x1b[31mFF\x1b[0m 04 05 06 07 09 ........"
        );
        // After a second the mark goes, and the row is written again.
        s.on_frame(&frame("124#41424344"), 1000);
        let later = s.draw(1300);
        assert!(
            later.contains("00257 | ---- 123 | 01 02 FF 04 05 06 07 09 ........"),
            "{later:?}"
        );
        assert!(!later.contains("\x1b[31m"));
    }

    #[test]
    fn the_data_can_be_shown_in_binary_and_without_text() {
        let mut s = seen();
        s.command("b");
        assert_eq!(
            rows(&mut s, 300),
            [
                "00257 | ---- 123 | 0000000100000010111111110000010000000101000001100000011100001001",
                "99999 | ---- 124 | 01000001010000100100001101000100",
                "99999 | ---- 7FF | ",
                "99999 | 12345678 | 1010101010111011",
            ]
        );
        s.command("b");
        s.command("B");
        assert_eq!(
            rows(&mut s, 300)[1],
            "99999 | ---- 124 | 01000001 01000010 01000011 01000100 "
        );
        s.command("8");
        let screen = s.draw(300);
        assert!(screen.starts_with(
            "\x1b[2J\x1b[HXX|ms|-- ID --|data ...     < vcan9 # l=20 h=100 t=500 slots=4 >"
        ));
        assert!(screen.contains("99999|---- 124|01000001010000100100001101000100\n"));
        assert!(screen.contains("99999|12345678|1010101010111011\n"));
        s.command("8");
        s.command("@");
        assert_eq!(
            rows(&mut s, 300),
            [
                "00257 | ---- 123 | 01 02 FF 04 05 06 07 09 ",
                "99999 | ---- 124 | 41 42 43 44 ",
                "99999 | ---- 7FF | ",
                "99999 | 12345678 | AA BB ",
            ]
        );
    }

    #[test]
    fn rows_go_when_their_identifier_is_silent_or_removed() {
        let mut s = seen();
        // Five seconds without a frame.
        s.on_frame(&frame("124#41424344"), 4000);
        let screen = s.draw(5300);
        assert!(screen.contains("slots=1 >"), "{screen:?}");
        // With the timeout off they stay.
        let mut s = seen();
        s.timeout = 0;
        assert!(s.draw(100_000).contains("slots=4 >"));
        // Removed and added by identifier, and by identifier and mask.
        s.command("-124");
        assert!(s.draw(0).contains("slots=3 >"));
        s.on_frame(&frame("124#01"), 0);
        assert!(s.draw(0).ends_with("\x1b[B\x1b[B\x1b[B"));
        s.command("+124");
        s.on_frame(&frame("124#01"), 0);
        assert!(s.draw(0).contains("slots=4 >"));
        s.command("-100700");
        assert!(s.draw(0).contains("slots=2 >"));
        s.command("N");
        assert!(s.draw(0).contains("slots=1 >"));
        s.command("n");
        s.command("+12345678000000FF");
        for text in ["123#01", "7FF#", "11111178#01", "11111179#01"] {
            s.on_frame(&frame(text), 0);
        }
        let screen = s.draw(0);
        assert!(screen.contains("slots=1 >") && screen.contains("| 11111178 |"));
        assert_eq!(s.command("q"), Reply::Quit);
        // Started quiet, nothing is shown until it is asked for.
        let mut s = Sniffer::new("vcan9");
        s.quiet();
        s.on_frame(&frame("123#01"), 0);
        assert!(s.draw(0).contains("slots=0 >"));
        s.command("+123");
        s.on_frame(&frame("123#01"), 0);
        assert!(s.draw(0).contains("slots=1 >"));
    }

    #[test]
    fn notched_bits_no_longer_count_as_changes() {
        let mut s = seen();
        s.colour = true;
        // The third byte of 123 has just changed: notch it.
        s.command("#");
        s.draw(2000);
        // It goes back to what it was: the same bits change again.
        s.on_frame(&frame("123#0102030405060709"), 2100);
        assert!(!rows(&mut s, 2200)[0].contains("\x1b[31m"));
        // Another byte still counts.
        s.on_frame(&frame("123#0102030405060700"), 2300);
        assert!(rows(&mut s, 2400)[0].contains("\x1b[1m\x1b[31m00\x1b[0m ........"));
        // Cleared, the third byte counts again.
        s.command("*");
        s.draw(4000);
        s.on_frame(&frame("123#0102FF0405060700"), 4100);
        assert!(rows(&mut s, 4200)[0].contains("\x1b[1m\x1b[31mFF\x1b[0m"));
    }

    #[test]
    fn settings_are_written_and_read_as_can_utils_has_them() {
        let mut s = seen();
        assert_eq!(
            s.command("wtest"),
            Reply::Write(
                "test".into(),
                "<00000123>1.0000000000000000\n\
                 <00000124>1.0000000000000000\n\
                 <000007FF>1.0000000000000000\n\
                 <92345678>1.0000000000000000\n"
                    .into()
            )
        );
        assert_eq!(s.command("rtest"), Reply::Read("test".into()));
        // A file that turns one identifier off and notches a byte of another.
        s.load("<00000124>0.0000000000000000\n<00000123>1.0000FF0000000000\n");
        assert!(s.draw(300).contains("slots=3 >"));
        s.colour = true;
        s.draw(2000);
        s.on_frame(&frame("123#0102000405060709"), 2100);
        assert!(!rows(&mut s, 2200)[0].contains("\x1b[31m"));
    }
}
