// SPDX-License-Identifier: MIT OR Apache-2.0

//! Address claiming for `j1939acd`: what a controller application does to
//! claim an address and to keep it, after SAE J1939-81, 5.9, with the
//! choices can-utils' `j1939acd` makes where the standard leaves them
//! open. No I/O and no clock: the caller passes in what was received and
//! the time, and carries out the actions it gets back.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use darwincan::j1939::{NO_PGN, PGN_ADDRESS_CLAIMED, PGN_REQUEST};
use darwincan::{J1939Addr, J1939Filter, J1939Socket};

/// How long the claims of others are waited for after asking for them,
/// before an address is chosen.
pub const REQUEST_WAIT_MS: u64 = 1250;
/// The address of a controller application that has none.
pub const NULL_ADDR: u8 = 0xFE;
/// The highest address that can be claimed.
pub const MAX_ADDR: u8 = 0xFD;

/// What the caller is to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send a request for address claimed to everyone, from the null
    /// address.
    Request,
    /// Send an address claim for this address.
    Claim(u8),
    /// Send a cannot claim address message.
    CannotClaim,
    /// Print this when asked to be verbose.
    Say(String),
    /// Print this when asked to be very verbose.
    SayMore(String),
    /// End, with this exit status and, for a failure, this message.
    Exit(u8, Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// The claims of others are being collected until this time.
    Asking(u64),
    Claimed,
    Ended,
}

/// One NAME's claim to an address.
pub struct Claimer {
    name: u64,
    allowed: [bool; 254],
    preferred: Option<u8>,
    /// The NAME last seen claiming each address; zero for none.
    holders: [u64; 254],
    addr: u8,
    state: State,
}

/// Parse a list of address ranges as `j1939acd -r` takes it:
/// `80,50-100,0x80-0x82`. Numbers are decimal, or hexadecimal with `0x`.
pub fn parse_ranges(s: &str) -> Result<[bool; 254], String> {
    let number = |t: &str| -> Result<usize, String> {
        let t = t.trim();
        let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            Some(hex) => usize::from_str_radix(hex, 16),
            None => t.parse(),
        };
        parsed.map_err(|_| format!("'{t}' is not an address"))
    };
    let mut allowed = [false; 254];
    for part in s.split(',').filter(|p| !p.trim().is_empty()) {
        let (first, last) = match part.split_once('-') {
            Some((a, b)) => (number(a)?, number(b)?),
            None => (number(part)?, number(part)?),
        };
        // Addresses above 253 cannot be claimed and are left out, as the
        // original leaves them out.
        for slot in allowed.iter_mut().take(last + 1).skip(first) {
            *slot = true;
        }
    }
    Ok(allowed)
}

impl Claimer {
    /// `preferred` is the address to start with if it is in `allowed` and
    /// nobody has it: the one given on the command line or kept from the
    /// last run.
    pub fn new(name: u64, allowed: [bool; 254], preferred: Option<u8>) -> Claimer {
        Claimer {
            name,
            allowed,
            preferred,
            holders: [0; 254],
            addr: NULL_ADDR,
            state: State::Idle,
        }
    }

    /// The address held, or [`NULL_ADDR`].
    pub fn address(&self) -> u8 {
        self.addr
    }

    /// When [`on_timer`](Self::on_timer) is to be called.
    pub fn deadline(&self) -> Option<u64> {
        match self.state {
            State::Asking(until) => Some(until),
            _ => None,
        }
    }

    pub fn ended(&self) -> bool {
        self.state == State::Ended
    }

    /// Begin: ask who has which address, and listen for a while. An
    /// address is claimed once the answers are in, so that one in use is
    /// not disturbed (SAE J1939-81, 5.9.7).
    pub fn start(&mut self, now_ms: u64) -> Vec<Action> {
        self.state = State::Asking(now_ms + REQUEST_WAIT_MS);
        vec![
            Action::Request,
            Action::Say(format!("request sent, pending for {REQUEST_WAIT_MS} ms")),
        ]
    }

    /// The address to claim next: the preferred one or the lowest allowed
    /// one that nobody has; failing that, the lowest one whose holder has
    /// a higher NAME and so has to give way.
    fn choose(&self) -> Option<u8> {
        let free = |a: usize| self.holders[a] == 0 || self.holders[a] == self.name;
        let allowed = |a: &usize| self.allowed[*a];
        if let Some(p) = self.preferred.map(usize::from) {
            if p < 254 && self.allowed[p] && free(p) {
                return Some(p as u8);
            }
        }
        (0..254)
            .filter(allowed)
            .find(|a| free(*a))
            .or_else(|| {
                (0..254)
                    .filter(allowed)
                    .find(|a| self.holders[*a] > self.name)
            })
            .map(|a| a as u8)
    }

    fn claim(&mut self, addr: u8) -> Vec<Action> {
        self.addr = addr;
        self.holders[usize::from(addr)] = self.name;
        self.state = State::Claimed;
        vec![
            Action::Claim(addr),
            Action::Say(format!("claimed 0x{addr:02x}")),
        ]
    }

    pub fn on_timer(&mut self, now_ms: u64) -> Vec<Action> {
        match self.state {
            State::Asking(until) if now_ms >= until => match self.choose() {
                Some(addr) => self.claim(addr),
                None => {
                    self.state = State::Ended;
                    vec![Action::Exit(1, Some("no free address to use".into()))]
                }
            },
            _ => Vec::new(),
        }
    }

    /// An address claim (or, from the null address, a cannot claim
    /// address message) from another node was received.
    pub fn on_claim(&mut self, sa: u8, name: u64) -> Vec<Action> {
        if self.state == State::Ended || self.state == State::Idle {
            return Vec::new();
        }
        if name == self.name {
            // Somebody else speaks for this NAME: the address it claims
            // is this NAME's address from now on.
            if self.state == State::Claimed && sa <= MAX_ADDR && sa != self.addr {
                self.holders[usize::from(self.addr)] = 0;
                self.addr = sa;
                self.holders[usize::from(sa)] = name;
            }
            return match self.state {
                State::Claimed if sa <= MAX_ADDR => {
                    vec![Action::Say(format!("claimed 0x{sa:02x}"))]
                }
                _ => Vec::new(),
            };
        }
        // A NAME has one address: the one it claimed last.
        for holder in self.holders.iter_mut().filter(|h| **h == name) {
            *holder = 0;
        }
        if sa > MAX_ADDR {
            return Vec::new();
        }
        if self.state != State::Claimed || sa != self.addr {
            self.holders[usize::from(sa)] = name;
            return Vec::new();
        }
        // A contending claim for this NAME's address: the lower NAME has
        // it (SAE J1939-81, 5.9.6).
        if name > self.name {
            return self.claim(sa);
        }
        self.holders[usize::from(sa)] = name;
        let mut actions = vec![Action::Say(format!("address collision for 0x{sa:02x}"))];
        match self.choose() {
            Some(addr) => actions.extend(self.claim(addr)),
            None => {
                self.addr = NULL_ADDR;
                self.state = State::Ended;
                actions.extend([
                    Action::Say("shutdown".into()),
                    Action::CannotClaim,
                    Action::SayMore("no address left".into()),
                    Action::Exit(0, None),
                ]);
            }
        }
        actions
    }

    /// A request for address claimed was received, sent to `da`.
    pub fn on_request(&mut self, da: u8) -> Vec<Action> {
        // Not before an address has been claimed (SAE J1939-81, 5.9.4).
        if self.state == State::Claimed && (da == 0xFF || da == self.addr) {
            return self.claim(self.addr);
        }
        Vec::new()
    }

    /// The program is told to end: the address is given up, so that
    /// others stop sending to it. [`address`](Self::address) still gives
    /// it, to be kept for the next run.
    pub fn shutdown(&mut self) -> Vec<Action> {
        let had = self.state == State::Claimed;
        self.state = State::Ended;
        let mut actions = vec![Action::Say("shutdown".into())];
        if had {
            actions.push(Action::CannotClaim);
        }
        actions.push(Action::Exit(0, None));
        actions
    }
}

/// Claim an address for `name` on `sock`, which is bound to the NAME and
/// to no address, and defend it until `stop` is set or no address is
/// left. What is said goes to `log`, by `verbose` (0 to 2). Returns the
/// exit status and, for a failure, the message.
pub fn run(
    sock: &J1939Socket,
    name: u64,
    claimer: &mut Claimer,
    stop: &AtomicBool,
    verbose: usize,
    log: &mut dyn Write,
) -> darwincan::Result<(u8, Option<String>)> {
    let bound = |addr: u8| J1939Addr {
        name,
        pgn: NO_PGN,
        addr,
    };
    let claim_from = |addr: u8| -> darwincan::Result<()> {
        sock.bind(bound(addr))?;
        sock.send_to(
            &name.to_le_bytes(),
            &J1939Addr::broadcast(PGN_ADDRESS_CLAIMED),
        )
    };
    let request = PGN_ADDRESS_CLAIMED.to_le_bytes();
    let request = &request[..3];
    // Carry out what the claimer asks for; the exit status once it asks
    // to end.
    let mut act = |actions: Vec<Action>| -> darwincan::Result<Option<(u8, Option<String>)>> {
        for action in actions {
            match action {
                Action::Request => sock.send_to(request, &J1939Addr::broadcast(PGN_REQUEST))?,
                Action::Claim(addr) => claim_from(addr)?,
                Action::CannotClaim => claim_from(NULL_ADDR)?,
                Action::Say(text) if verbose >= 1 => {
                    let _ = writeln!(log, "- {text}");
                }
                Action::SayMore(text) if verbose >= 2 => {
                    let _ = writeln!(log, "{text}");
                }
                Action::Say(_) | Action::SayMore(_) => {}
                Action::Exit(code, message) => {
                    // The last message is on the bus before the caller
                    // closes the socket.
                    sock.flush()?;
                    return Ok(Some((code, message)));
                }
            }
        }
        Ok(None)
    };
    let started = Instant::now();
    let now_ms = || started.elapsed().as_millis() as u64;
    sock.set_broadcast(true)?;
    sock.set_filters(&[
        J1939Filter::pgn(PGN_ADDRESS_CLAIMED),
        J1939Filter::pgn(PGN_REQUEST),
    ])?;
    if let Some(end) = act(claimer.start(now_ms()))? {
        return Ok(end);
    }
    loop {
        if stop.load(Ordering::Relaxed) {
            if let Some(end) = act(claimer.shutdown())? {
                return Ok(end);
            }
        }
        // Wake often enough to notice that it is time to stop.
        let mut wait = Duration::from_millis(100);
        if let Some(deadline) = claimer.deadline() {
            wait = wait.min(Duration::from_millis(
                deadline.saturating_sub(now_ms()).max(1),
            ));
        }
        sock.set_read_timeout(Some(wait))?;
        let actions = match sock.recv() {
            Ok(m) if m.src.pgn == PGN_ADDRESS_CLAIMED && m.data.len() == 8 => {
                let theirs = u64::from_le_bytes(m.data[..].try_into().expect("eight bytes"));
                claimer.on_claim(m.src.addr, theirs)
            }
            Ok(m) if m.src.pgn == PGN_REQUEST && m.data[..] == *request => {
                claimer.on_request(m.dst_addr)
            }
            Ok(_) => Vec::new(),
            Err(e) if e.is_timeout() => Vec::new(),
            Err(e) => return Err(e),
        };
        if let Some(end) = act(actions)? {
            return Ok(end);
        }
        if let Some(end) = act(claimer.on_timer(now_ms()))? {
            return Ok(end);
        }
    }
}

/// The text of the file an address is kept in between runs.
pub fn cache_text(addr: u8, saved_on: &str) -> String {
    format!("# saved on {saved_on}\n\n\n0x{addr:02x}\n")
}

/// The address in such a file: the first line that is not a comment.
pub fn parse_cache(text: &str) -> Option<u8> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    let value = match line.strip_prefix("0x").or_else(|| line.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16).ok()?,
        None => line.parse().ok()?,
    };
    (value <= MAX_ADDR).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: u64 = 0x1122_3344_5566_7788;
    const LOWER: u64 = 1;
    const HIGHER: u64 = u64::MAX;

    fn says(actions: &[Action]) -> Vec<&str> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Say(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A claimer for 0x80 to 0x82 that has claimed its first address.
    fn claimed() -> Claimer {
        let mut c = Claimer::new(NAME, parse_ranges("0x80-0x82").unwrap(), None);
        c.start(0);
        assert_eq!(c.on_timer(1250)[0], Action::Claim(0x80));
        c
    }

    #[test]
    fn ranges_are_decimal_unless_marked_hexadecimal() {
        let r = parse_ranges("100,80-82").unwrap();
        let set: Vec<usize> = (0..254).filter(|a| r[*a]).collect();
        assert_eq!(set, [80, 81, 82, 100]);
        let r = parse_ranges("0x80-0x82").unwrap();
        assert_eq!((0..254).filter(|a| r[*a]).count(), 3);
        assert!(r[0x80] && r[0x82]);
        // Nothing above 253 can be claimed.
        assert_eq!(parse_ranges("300").unwrap(), [false; 254]);
        assert_eq!(
            parse_ranges("250-255")
                .unwrap()
                .iter()
                .filter(|a| **a)
                .count(),
            4
        );
        assert!(parse_ranges("8x").is_err());
    }

    #[test]
    fn it_asks_first_and_claims_after_the_wait() {
        let mut c = Claimer::new(NAME, parse_ranges("0x80-0x82").unwrap(), None);
        let begun = c.start(1000);
        assert_eq!(begun[0], Action::Request);
        assert_eq!(says(&begun), ["request sent, pending for 1250 ms"]);
        assert_eq!(c.deadline(), Some(2250));
        assert!(c.on_timer(2249).is_empty());
        // No request is answered before an address has been claimed.
        assert!(c.on_request(0xFF).is_empty());
        let actions = c.on_timer(2250);
        assert_eq!(actions[0], Action::Claim(0x80));
        assert_eq!(says(&actions), ["claimed 0x80"]);
        assert_eq!((c.address(), c.deadline()), (0x80, None));
    }

    #[test]
    fn an_address_somebody_has_is_left_alone_at_first() {
        let mut c = Claimer::new(NAME, parse_ranges("0x80-0x82").unwrap(), None);
        c.start(0);
        c.on_claim(0x80, LOWER);
        c.on_claim(0x81, HIGHER);
        assert_eq!(c.on_timer(1250)[0], Action::Claim(0x82));
    }

    #[test]
    fn the_preferred_address_is_taken_if_it_is_allowed_and_free() {
        let allowed = parse_ranges("0x80-0x82").unwrap();
        let mut c = Claimer::new(NAME, allowed, Some(0x81));
        c.start(0);
        assert_eq!(c.on_timer(1250)[0], Action::Claim(0x81));
        // Outside the range it is passed over.
        let mut c = Claimer::new(NAME, allowed, Some(0x90));
        c.start(0);
        assert_eq!(c.on_timer(1250)[0], Action::Claim(0x80));
        // And so it is when somebody has it.
        let mut c = Claimer::new(NAME, allowed, Some(0x81));
        c.start(0);
        c.on_claim(0x81, HIGHER);
        assert_eq!(c.on_timer(1250)[0], Action::Claim(0x80));
    }

    #[test]
    fn with_nothing_free_a_higher_name_has_to_give_way() {
        let mut c = Claimer::new(NAME, parse_ranges("0x80").unwrap(), None);
        c.start(0);
        c.on_claim(0x80, HIGHER);
        assert_eq!(c.on_timer(1250)[0], Action::Claim(0x80));
        // A lower NAME does not: there is no address to use.
        let mut c = Claimer::new(NAME, parse_ranges("0x80").unwrap(), None);
        c.start(0);
        c.on_claim(0x80, LOWER);
        assert_eq!(
            c.on_timer(1250),
            [Action::Exit(1, Some("no free address to use".into()))]
        );
        assert!(c.ended());
    }

    #[test]
    fn a_higher_name_is_answered_and_a_lower_one_given_way_to() {
        let mut c = claimed();
        // The higher NAME is told again whose address it is.
        let actions = c.on_claim(0x80, HIGHER);
        assert_eq!(actions[0], Action::Claim(0x80));
        assert_eq!(says(&actions), ["claimed 0x80"]);
        // The lower NAME gets it; the next address is claimed.
        let actions = c.on_claim(0x80, LOWER);
        assert_eq!(
            says(&actions),
            ["address collision for 0x80", "claimed 0x81"]
        );
        assert!(actions.contains(&Action::Claim(0x81)));
        assert_eq!(c.address(), 0x81);
        // Claims for other addresses are only noted.
        assert!(c.on_claim(0x82, LOWER + 1).is_empty());
        // With 0x80 and 0x82 taken by lower NAMEs, losing 0x81 leaves
        // nothing: the NAME says so and the program ends.
        let actions = c.on_claim(0x81, LOWER + 2);
        assert_eq!(
            actions,
            [
                Action::Say("address collision for 0x81".into()),
                Action::Say("shutdown".into()),
                Action::CannotClaim,
                Action::SayMore("no address left".into()),
                Action::Exit(0, None),
            ]
        );
        assert_eq!(c.address(), NULL_ADDR);
        assert!(c.ended());
    }

    #[test]
    fn a_name_that_moves_frees_the_address_it_had() {
        let mut c = claimed();
        c.on_claim(0x81, LOWER);
        // The lower NAME takes 0x80 and so no longer has 0x81.
        let actions = c.on_claim(0x80, LOWER);
        assert!(actions.contains(&Action::Claim(0x81)));
        // One that cannot claim an address has none either.
        let mut c = claimed();
        c.on_claim(0x81, LOWER);
        c.on_claim(NULL_ADDR, LOWER);
        assert!(c.on_claim(0x80, LOWER + 1).contains(&Action::Claim(0x81)));
    }

    #[test]
    fn requests_are_answered_with_the_claim() {
        let mut c = claimed();
        assert_eq!(c.on_request(0xFF)[0], Action::Claim(0x80));
        assert_eq!(c.on_request(0x80)[0], Action::Claim(0x80));
        assert!(c.on_request(0x81).is_empty());
    }

    #[test]
    fn a_claim_made_for_this_name_elsewhere_is_taken_over() {
        let mut c = claimed();
        assert_eq!(c.on_claim(0x90, NAME), [Action::Say("claimed 0x90".into())]);
        assert_eq!(c.address(), 0x90);
        assert_eq!(c.on_request(0x90)[0], Action::Claim(0x90));
    }

    #[test]
    fn ending_gives_the_address_up_and_keeps_it_for_the_next_run() {
        let mut c = claimed();
        assert_eq!(
            c.shutdown(),
            [
                Action::Say("shutdown".into()),
                Action::CannotClaim,
                Action::Exit(0, None)
            ]
        );
        assert_eq!(c.address(), 0x80);
        // Before an address was claimed there is nothing to give up.
        let mut c = Claimer::new(NAME, parse_ranges("0x80").unwrap(), None);
        c.start(0);
        assert!(!c.shutdown().contains(&Action::CannotClaim));
    }

    #[test]
    fn the_address_is_kept_in_a_file_as_the_original_writes_it() {
        let text = cache_text(0x80, "Mon Nov 16 08:43:39 2026");
        assert_eq!(text, "# saved on Mon Nov 16 08:43:39 2026\n\n\n0x80\n");
        assert_eq!(parse_cache(&text), Some(0x80));
        assert_eq!(parse_cache("0x82\n"), Some(0x82));
        assert_eq!(parse_cache("130\n"), Some(130));
        // No address was held when it was written, or it is not one.
        assert_eq!(parse_cache(&cache_text(NULL_ADDR, "x")), None);
        assert_eq!(parse_cache("# nothing\n"), None);
        assert_eq!(parse_cache("junk\n"), None);
    }
}
