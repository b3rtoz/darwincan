// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use darwincan_core::wire::LinkState;
use darwincan_core::{err, BusConfig, Frame};

use crate::clock::now_ns;
use crate::timing;
use crate::Backend;

/// Something attached to a bus that wants to see its frames: a client's raw
/// socket or an ISO-TP channel.
pub(crate) trait Sink: Send + Sync {
    fn id(&self) -> u64;
    /// `own` is true when this sink transmitted the frame itself. Must not
    /// block: it runs on the sender's (or the hardware receive) thread.
    fn deliver(&self, frame: &Frame, timestamp_ns: u64, own: bool);
    /// The interface went away (adapter unplugged).
    fn bus_removed(&self) {}
    /// A frame this sink transmitted was handed to the adapter but will
    /// not be reported sent: the interface went down or bus-off first.
    fn transmit_dropped(&self) {}
    /// Whether the interface can send has changed.
    fn link_state(&self, _state: LinkState) {}
}

/// Who sent a frame that the adapter has yet to report as sent.
struct Pending {
    origin: u64,
    loopback: bool,
}

pub(crate) enum ConfigureError {
    /// A virtual bus has no bus settings.
    Virtual,
    /// The interface is on the bus.
    Up,
}

/// One CAN interface: a virtual bus, or a hardware channel behind a backend.
pub(crate) struct Bus {
    name: RwLock<String>,
    /// Hardware only: which adapter channel this is, for keeping its name.
    pub identity: Option<String>,
    /// The interface's number among the daemon's, from 1.
    pub index: u32,
    backend: Option<Box<dyn Backend>>,
    sinks: RwLock<Vec<Arc<dyn Sink>>>,
    /// Hardware only: the configuration in use while on the bus, kept
    /// after going off it for the next start. Also serialises start/stop.
    config: Mutex<Option<BusConfig>>,
    /// Whether the interface is on the bus.
    up: AtomicBool,
    /// Hardware only: the bitrates in use, for [`Bus::frame_time_ns`].
    /// Zero when there is none.
    bitrate: AtomicU32,
    data_bitrate: AtomicU32,
    removed: AtomicBool,
    /// Hardware only: the controller reported bus-off and has not been
    /// restarted since. It sends nothing in that state.
    bus_off: AtomicBool,
    /// Frames handed to a backend that confirms transmission and not yet
    /// reported sent, by token.
    pending: Mutex<HashMap<u32, Pending>>,
    next_token: AtomicU32,
}

impl Bus {
    pub fn with_identity(
        name: &str,
        index: u32,
        identity: Option<String>,
        backend: Option<Box<dyn Backend>>,
    ) -> Bus {
        Bus {
            name: RwLock::new(name.to_string()),
            identity,
            index,
            // A virtual bus has nothing to configure and is always up.
            up: AtomicBool::new(backend.is_none()),
            backend,
            sinks: RwLock::new(Vec::new()),
            config: Mutex::new(None),
            bitrate: AtomicU32::new(0),
            data_bitrate: AtomicU32::new(0),
            removed: AtomicBool::new(false),
            bus_off: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            next_token: AtomicU32::new(0),
        }
    }

    pub fn name(&self) -> String {
        self.name.read().unwrap().clone()
    }

    /// Call the interface something else. The caller keeps the list of
    /// interfaces in step.
    pub fn set_name(&self, name: &str) {
        *self.name.write().unwrap() = name.to_string();
    }

    pub fn kind(&self) -> &str {
        match &self.backend {
            Some(b) => b.kind(),
            None => "virtual",
        }
    }

    pub fn description(&self) -> String {
        match &self.backend {
            Some(b) => b.description(),
            None => String::new(),
        }
    }

    pub fn config(&self) -> Option<BusConfig> {
        *self.config.lock().unwrap()
    }

    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Acquire)
    }

    pub fn is_virtual(&self) -> bool {
        self.backend.is_none()
    }

    /// Whether clients see a frame they sent when the adapter reports it
    /// on the bus, rather than when it was handed over.
    pub fn confirms_transmission(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(|b| b.confirms_transmission())
    }

    /// Forget the frames the adapter has not reported sent, and tell
    /// their senders. Linux discards them the same way when an interface
    /// goes down.
    fn drop_pending(&self) {
        let mut origins: Vec<u64> = self
            .pending
            .lock()
            .unwrap()
            .drain()
            .map(|(_, p)| p.origin)
            .collect();
        if origins.is_empty() {
            return;
        }
        origins.sort_unstable();
        origins.dedup();
        for sink in self.sinks.read().unwrap().iter() {
            if origins.binary_search(&sink.id()).is_ok() {
                sink.transmit_dropped();
            }
        }
    }

    /// The adapter reports `frame` sent and acknowledged on the bus.
    pub fn transmitted(&self, token: u32, frame: &Frame, timestamp_ns: u64) {
        let sender = self.pending.lock().unwrap().remove(&token);
        for sink in self.sinks.read().unwrap().iter() {
            match &sender {
                Some(p) => {
                    let own = sink.id() == p.origin;
                    if own || p.loopback {
                        sink.deliver(frame, timestamp_ns, own);
                    }
                }
                // Sent before the interface was last restarted: nobody's
                // own any more, but it was on the bus.
                None => sink.deliver(frame, timestamp_ns, false),
            }
        }
    }

    /// Whether the interface can send, and if not, why.
    pub fn link_state(&self) -> LinkState {
        if self.removed.load(Ordering::Acquire) {
            LinkState::Removed
        } else if !self.up.load(Ordering::Acquire) {
            LinkState::Down
        } else if self.bus_off.load(Ordering::Acquire) {
            LinkState::BusOff
        } else {
            LinkState::Up
        }
    }

    /// Tell the sinks whether the interface can send, after a change.
    fn announce(&self) {
        let state = self.link_state();
        for sink in self.sinks.read().unwrap().iter() {
            sink.link_state(state);
        }
    }

    /// Remember `config` for the next start. Refused while on the bus, as
    /// Linux refuses to change the bit timing of an interface that is up.
    pub fn configure(&self, config: &BusConfig) -> Result<(), ConfigureError> {
        if self.backend.is_none() {
            return Err(ConfigureError::Virtual);
        }
        let mut current = self.config.lock().unwrap();
        if self.is_up() {
            return Err(ConfigureError::Up);
        }
        *current = Some(*config);
        Ok(())
    }

    /// Configure the hardware and go on the bus. Reconfigures if already up.
    pub fn start(&self, config: &BusConfig) -> io::Result<()> {
        let Some(backend) = &self.backend else {
            return Ok(());
        };
        let mut current = self.config.lock().unwrap();
        if self.is_up() {
            self.up.store(false, Ordering::Release);
            backend.stop()?;
            self.drop_pending();
        }
        backend.start(config)?;
        self.bus_off.store(false, Ordering::Release);
        *current = Some(*config);
        self.bitrate.store(config.bitrate, Ordering::Release);
        self.data_bitrate
            .store(config.data_bitrate.unwrap_or(0), Ordering::Release);
        self.up.store(true, Ordering::Release);
        drop(current);
        self.announce();
        Ok(())
    }

    /// Put a hardware interface that is up back on the bus with its
    /// settings, for when the adapter has lost them (the computer slept).
    /// Clients are told with a "controller restarted" error frame. `None`
    /// if there was nothing to do.
    pub fn restore(&self) -> Option<io::Result<()>> {
        let backend = self.backend.as_ref()?;
        let current = self.config.lock().unwrap();
        let config = (*current)?;
        if !self.is_up() || self.removed.load(Ordering::Acquire) {
            return None;
        }
        let result = backend.restore(&config);
        self.drop_pending();
        if result.is_ok() {
            self.receive(&Frame::new_error(err::RESTARTED, [0; 8]), now_ns());
        }
        Some(result)
    }

    /// How long `frame` occupies the wire at the most, in nanoseconds: it
    /// has left the bus this long after the adapter started sending it.
    /// Zero on a virtual bus.
    pub fn frame_time_ns(&self, frame: &Frame) -> u64 {
        let nominal = self.bitrate.load(Ordering::Acquire);
        if nominal == 0 {
            return 0;
        }
        let data = match self.data_bitrate.load(Ordering::Acquire) {
            0 => nominal,
            rate => rate,
        };
        let (slow, fast) = frame_bits(frame);
        slow * 1_000_000_000 / u64::from(nominal) + fast * 1_000_000_000 / u64::from(data)
    }

    /// Go off the bus.
    pub fn stop(&self) -> io::Result<()> {
        let Some(backend) = &self.backend else {
            return Ok(());
        };
        let _serialise = self.config.lock().unwrap();
        if self.is_up() {
            self.up.store(false, Ordering::Release);
            self.announce();
            let stopped = backend.stop();
            self.drop_pending();
            stopped?;
        }
        Ok(())
    }

    /// Attach a sink. `acknowledge` runs while frame delivery is held off,
    /// so whatever it queues for the client precedes the first frame, and
    /// by the time the client can act on it the sink is attached.
    pub fn add_sink(&self, sink: Arc<dyn Sink>, acknowledge: impl FnOnce()) {
        let mut sinks = self.sinks.write().unwrap();
        acknowledge();
        sinks.push(sink);
    }

    pub fn remove_sink(&self, id: u64) {
        self.sinks.write().unwrap().retain(|s| s.id() != id);
    }

    /// The adapter is gone: fail further transmits and tell the clients.
    pub fn mark_removed(&self) {
        self.removed.store(true, Ordering::Release);
        self.up.store(false, Ordering::Release);
        self.drop_pending();
        for sink in self.sinks.read().unwrap().iter() {
            sink.link_state(LinkState::Removed);
            sink.bus_removed();
        }
    }

    /// Transmit a frame on behalf of sink `origin`: hand it to the hardware
    /// (if any) and echo it to local sinks. `loopback` controls whether
    /// other local sinks see it; the origin's own copy is always offered
    /// and the sink decides whether it wants it. Behind a backend that
    /// confirms transmission the echo is not made here: it follows from
    /// [`Bus::transmitted`], when the frame has been on the bus.
    pub fn transmit(&self, origin: u64, loopback: bool, frame: &Frame) -> io::Result<()> {
        if self.removed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} was removed", self.name()),
            ));
        }
        if !self.up.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("{} is down", self.name()),
            ));
        }
        // A controller that is bus-off takes frames and sends none of
        // them, so refuse here rather than echo a frame that never was on
        // the bus. Linux fails such a send with "network is down" too.
        if self.bus_off.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("{} is bus-off; take it down and up again", self.name()),
            ));
        }
        if let Some(backend) = &self.backend {
            let confirms = backend.confirms_transmission();
            let token = self.next_token.fetch_add(1, Ordering::Relaxed);
            if confirms {
                // Before the hand-over: the report can come back at once.
                self.pending
                    .lock()
                    .unwrap()
                    .insert(token, Pending { origin, loopback });
            }
            let started = timing::start();
            let handed_over = backend.transmit(frame, token);
            timing::finish(started, || {
                format!("{}: handing a frame to the adapter took", self.name())
            });
            if confirms {
                if handed_over.is_err() {
                    self.pending.lock().unwrap().remove(&token);
                }
                return handed_over;
            }
            handed_over?;
        }
        let ts = now_ns();
        for sink in self.sinks.read().unwrap().iter() {
            let own = sink.id() == origin;
            if own || loopback {
                sink.deliver(frame, ts, own);
            }
        }
        Ok(())
    }

    /// Distribute a frame received from the hardware.
    pub fn receive(&self, frame: &Frame, timestamp_ns: u64) {
        if frame.is_error() {
            let class = frame.can_id();
            if class & err::BUSOFF != 0 {
                if !self.bus_off.swap(true, Ordering::AcqRel) {
                    self.announce();
                }
                self.drop_pending();
            } else if (class & err::RESTARTED != 0
                || (class & err::CRTL != 0 && frame.data()[1] & err::CRTL_ACTIVE != 0))
                && self.bus_off.swap(false, Ordering::AcqRel)
            {
                self.announce();
            }
        }
        for sink in self.sinks.read().unwrap().iter() {
            sink.deliver(frame, timestamp_ns, false);
        }
    }
}

/// The most bits `frame` can take on the wire, stuff bits included, split
/// into those sent at the nominal bitrate and those sent at the data
/// bitrate (none unless the frame switches bitrate).
fn frame_bits(frame: &Frame) -> (u64, u64) {
    let len = frame.data().len() as u64;
    let extended = frame.is_extended();
    if !frame.is_fd() {
        // Start of frame to the end of the CRC is stuffed: one extra bit
        // for every four at worst. The CRC delimiter, acknowledge slot,
        // end of frame and intermission (13 bits) are not.
        let payload = if frame.is_remote() { 0 } else { 8 * len };
        let stuffed = if extended { 54 } else { 34 } + payload;
        return (stuffed + (stuffed - 1) / 4 + 13, 0);
    }
    // CAN FD: arbitration up to the bit rate switch, then control, data
    // and CRC, then the acknowledge and end of frame at the nominal rate.
    let arbitration = if extended { 35 } else { 16 };
    let crc = if len > 16 { 21 } else { 17 };
    // Length code, data, the stuff count and the CRC with a fixed stuff
    // bit before every four of its bits.
    let body = 6 + 8 * len;
    let fast = body + (body + arbitration) / 4 + 5 + crc + crc / 4 + 2;
    let slow = arbitration + arbitration / 4 + 13;
    if frame.is_brs() {
        (slow, fast)
    } else {
        (slow + fast, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darwincan_core::Id;

    #[test]
    fn classic_frames_are_counted_with_worst_case_stuffing() {
        // The well-known maxima: 135 bits for a full standard frame and
        // 160 for a full extended one, including the intermission.
        let f = Frame::new(Id::Standard(0x123), &[0; 8]).unwrap();
        assert_eq!(frame_bits(&f), (135, 0));
        let f = Frame::new(Id::Extended(0x123), &[0; 8]).unwrap();
        assert_eq!(frame_bits(&f), (160, 0));
        let f = Frame::new(Id::Standard(0x123), &[]).unwrap();
        assert_eq!(frame_bits(&f), (55, 0));
        // A remote frame carries no data whatever length it asks for.
        let f = Frame::new_remote(Id::Standard(0x123), 8).unwrap();
        assert_eq!(frame_bits(&f), (55, 0));
    }

    #[test]
    fn fd_frames_split_at_the_bit_rate_switch() {
        let brs = Frame::new_fd(Id::Standard(0x123), &[0; 64], true).unwrap();
        let (slow, fast) = frame_bits(&brs);
        // 64 data bytes dominate the fast part.
        assert!((512..700).contains(&fast), "{fast}");
        assert!((25..40).contains(&slow), "{slow}");
        // Without the switch the same bits all go at the nominal rate.
        let plain = Frame::new_fd(Id::Standard(0x123), &[0; 64], false).unwrap();
        assert_eq!(frame_bits(&plain), (slow + fast, 0));
    }

    #[test]
    fn frame_time_follows_the_bitrate() {
        let f = Frame::new(Id::Standard(0x123), &[0; 8]).unwrap();
        let bus = Bus::with_identity("vcan9", 1, None, None);
        assert_eq!(bus.frame_time_ns(&f), 0);
        bus.bitrate.store(500_000, Ordering::Release);
        assert_eq!(bus.frame_time_ns(&f), 270_000);
        // A frame that switches to 2 Mbit/s is shorter than one that does not.
        bus.data_bitrate.store(2_000_000, Ordering::Release);
        let brs = Frame::new_fd(Id::Standard(0x123), &[0; 8], true).unwrap();
        let plain = Frame::new_fd(Id::Standard(0x123), &[0; 8], false).unwrap();
        assert!(bus.frame_time_ns(&brs) < bus.frame_time_ns(&plain));
    }
}
