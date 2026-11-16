//! ISO-TP (ISO 15765-2) for classic CAN and CAN FD, normal addressing.
//!
//! [`IsoTp`] is a sans-I/O state machine: it never touches a bus or a clock.
//! The owner feeds it received frames and the current time, and drains the
//! frames it wants transmitted and the events it produced:
//!
//! ```text
//! on_frame(frame, now) / send(data, now) / poll(now)
//!         -> poll_transmit()  frames to put on the bus
//!         -> poll_event()     completed messages and failures
//!         -> next_deadline()  when poll() must next be called
//! ```
//!
//! Times are nanoseconds on any monotonic clock. Transmission and reception
//! are independent, so one channel can do both at once.

use std::collections::VecDeque;

use darwincan_core::{
    fd_len_round_up, Frame, IsoTpFailure, IsoTpParams, CANFD_BRS, CANFD_FDF, CAN_ERR_FLAG,
    CAN_RTR_FLAG,
};

const PCI_SF: u8 = 0x0;
const PCI_FF: u8 = 0x1;
const PCI_CF: u8 = 0x2;
const PCI_FC: u8 = 0x3;

const FC_CTS: u8 = 0;
const FC_WAIT: u8 = 1;
const FC_OVERFLOW: u8 = 2;

/// Padding byte for CAN FD frames that must be rounded up to a valid length
/// when the channel has no padding configured.
const FD_FILL: u8 = 0xCC;
/// Consecutive flow control WAIT frames tolerated before giving up.
const MAX_WAIT_FRAMES: u32 = 16;
const NS_PER_MS: u64 = 1_000_000;
/// N_As: how long a frame may take to get onto the bus.
const N_AS_MS: u64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A complete message arrived.
    Received(Vec<u8>),
    /// The message passed to [`IsoTp::send`] was fully transmitted.
    TxDone,
    TxFailed(IsoTpFailure),
    RxFailed(IsoTpFailure),
}

#[derive(Debug, Clone, Copy)]
enum TxState {
    Idle,
    /// First frame (or a full block) sent; waiting for flow control.
    WaitFc {
        deadline: u64,
    },
    /// Sending consecutive frames; the next one is due at `next_at`.
    Sending {
        next_at: u64,
    },
}

#[derive(Debug, Clone, Copy)]
enum RxState {
    Idle,
    Receiving { deadline: u64 },
}

/// Decode an STmin byte to nanoseconds. Reserved values mean the maximum.
pub fn st_min_to_ns(raw: u8) -> u64 {
    match raw {
        0x00..=0x7F => u64::from(raw) * NS_PER_MS,
        0xF1..=0xF9 => u64::from(raw - 0xF0) * 100_000,
        _ => 0x7F * NS_PER_MS,
    }
}

pub struct IsoTp {
    p: IsoTpParams,

    tx_state: TxState,
    tx_data: Vec<u8>,
    tx_offset: usize,
    tx_sn: u8,
    tx_st_min_ns: u64,
    /// Consecutive frames left in the current block; `None` = unlimited.
    tx_block_left: Option<u8>,
    /// A consecutive frame the bus owner will confirm (its first byte),
    /// and when to give up waiting for that.
    tx_unconfirmed: Option<(u8, u64)>,
    tx_wait_frames: u32,

    rx_state: RxState,
    rx_buf: Vec<u8>,
    rx_total: usize,
    rx_sn: u8,
    rx_block_count: u8,

    out: VecDeque<Frame>,
    events: VecDeque<Event>,
}

impl IsoTp {
    /// `params` must have passed [`IsoTpParams::validate`].
    pub fn new(params: IsoTpParams) -> IsoTp {
        debug_assert!(params.validate().is_ok());
        IsoTp {
            p: params,
            tx_state: TxState::Idle,
            tx_data: Vec::new(),
            tx_offset: 0,
            tx_sn: 0,
            tx_st_min_ns: 0,
            tx_block_left: None,
            tx_unconfirmed: None,
            tx_wait_frames: 0,
            rx_state: RxState::Idle,
            rx_buf: Vec::new(),
            rx_total: 0,
            rx_sn: 0,
            rx_block_count: 0,
            out: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn params(&self) -> &IsoTpParams {
        &self.p
    }

    /// True while a multi-frame transmission is in progress.
    pub fn is_sending(&self) -> bool {
        !matches!(self.tx_state, TxState::Idle)
    }

    /// Next frame to put on the bus, if any.
    pub fn poll_transmit(&mut self) -> Option<Frame> {
        self.out.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// The time at which [`poll`](Self::poll) must be called again, if a
    /// timer is running.
    pub fn next_deadline(&self) -> Option<u64> {
        let tx = match self.tx_state {
            TxState::Idle => None,
            TxState::WaitFc { deadline } => Some(deadline),
            TxState::Sending { next_at } => match self.tx_unconfirmed {
                Some((_, give_up)) => Some(give_up),
                None => Some(next_at),
            },
        };
        let rx = match self.rx_state {
            RxState::Idle => None,
            RxState::Receiving { deadline } => Some(deadline),
        };
        match (tx, rx) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Start transmitting `data`. Completion is reported by
    /// [`Event::TxDone`] or [`Event::TxFailed`].
    pub fn send(&mut self, data: Vec<u8>, now: u64) -> Result<(), IsoTpFailure> {
        if self.is_sending() {
            return Err(IsoTpFailure::Busy);
        }
        if data.is_empty() || u32::try_from(data.len()).is_err() {
            return Err(IsoTpFailure::InvalidLength);
        }
        let dl = usize::from(self.p.tx_dl);
        let len = data.len();
        let sf_capacity = if dl <= 8 { 7 } else { dl - 2 };

        if len <= sf_capacity {
            let mut pdu = Vec::with_capacity(len + 2);
            if len <= 7 {
                pdu.push(len as u8);
            } else {
                // CAN FD single frame with escape: length in the second byte.
                pdu.push(0);
                pdu.push(len as u8);
            }
            pdu.extend_from_slice(&data);
            self.queue_frame(&pdu);
            self.events.push_back(Event::TxDone);
            return Ok(());
        }

        let mut pdu = Vec::with_capacity(dl);
        if len <= 4095 {
            pdu.push((PCI_FF << 4) | (len >> 8) as u8);
            pdu.push(len as u8);
        } else {
            // Escape sequence: 32-bit length follows.
            pdu.push(PCI_FF << 4);
            pdu.push(0);
            pdu.extend_from_slice(&(len as u32).to_be_bytes());
        }
        let n = dl - pdu.len();
        pdu.extend_from_slice(&data[..n]);
        self.queue_frame(&pdu);

        self.tx_data = data;
        self.tx_offset = n;
        self.tx_sn = 1;
        self.tx_wait_frames = 0;
        self.tx_state = TxState::WaitFc {
            deadline: now + u64::from(self.p.n_bs_ms) * NS_PER_MS,
        };
        Ok(())
    }

    /// Tell the state machine when a frame from
    /// [`poll_transmit`](Self::poll_transmit) actually went out.
    ///
    /// STmin is a gap on the bus, so the next consecutive frame is timed
    /// from the moment the previous one was sent, not from when it was
    /// queued. Without this call the gap is measured from the
    /// [`poll`](Self::poll) that produced the frame, which can undercut
    /// STmin by however long the frame waited to be sent.
    pub fn frame_transmitted(&mut self, frame: &Frame, now: u64) {
        let first = frame.data().first().copied();
        if self.tx_st_min_ns == 0 || first.map(|b| b >> 4) != Some(PCI_CF) {
            return;
        }
        if let Some((awaited, _)) = self.tx_unconfirmed {
            // Only the frame being waited for counts: an older one may
            // still be reported after its message was given up.
            if first != Some(awaited) {
                return;
            }
            self.tx_unconfirmed = None;
        }
        if let TxState::Sending { next_at } = &mut self.tx_state {
            *next_at = (*next_at).max(now + self.tx_st_min_ns);
        }
    }

    /// Tell the state machine that `frame`, just taken from
    /// [`poll_transmit`](Self::poll_transmit), was handed to a bus that
    /// reports when a frame has really been sent, and that
    /// [`frame_transmitted`](Self::frame_transmitted) will be called with
    /// that time. The next consecutive frame then waits for the report,
    /// so STmin holds on the bus however long the frame took to get
    /// there. If no report comes within N_As (one second) the
    /// transmission fails with [`IsoTpFailure::NotSent`].
    pub fn frame_pending(&mut self, frame: &Frame, now: u64) {
        let first = frame.data().first().copied();
        if self.tx_st_min_ns == 0 || first.map(|b| b >> 4) != Some(PCI_CF) {
            return;
        }
        if let (TxState::Sending { .. }, Some(first)) = (self.tx_state, first) {
            self.tx_unconfirmed = Some((first, now + N_AS_MS * NS_PER_MS));
        }
    }

    /// Give up a transmission in progress because one of its frames could
    /// not be put on the bus.
    pub fn abort_send(&mut self) {
        if self.is_sending() {
            self.fail_tx(IsoTpFailure::NotSent);
        }
    }

    /// Process a frame received from the bus. Frames for other identifiers
    /// are ignored, so the caller need not pre-filter.
    pub fn on_frame(&mut self, frame: &Frame, now: u64) {
        if frame.can_id() != self.p.rx_id || frame.can_id() & (CAN_RTR_FLAG | CAN_ERR_FLAG) != 0 {
            return;
        }
        let d = frame.data();
        if d.is_empty() {
            return;
        }
        match d[0] >> 4 {
            PCI_SF => self.on_single(d),
            PCI_FF => self.on_first(d, now),
            PCI_CF => self.on_consecutive(d, now),
            PCI_FC => self.on_flow_control(d, now),
            _ => {}
        }
        self.poll(now);
    }

    /// Advance timers: send consecutive frames that are due and report
    /// timeouts.
    pub fn poll(&mut self, now: u64) {
        if let RxState::Receiving { deadline } = self.rx_state {
            if now >= deadline {
                self.fail_rx(IsoTpFailure::Timeout);
            }
        }
        match self.tx_state {
            TxState::Idle => {}
            TxState::WaitFc { deadline } => {
                if now >= deadline {
                    self.fail_tx(IsoTpFailure::Timeout);
                }
            }
            TxState::Sending { next_at } => match self.tx_unconfirmed {
                Some((_, give_up)) => {
                    if now >= give_up {
                        self.fail_tx(IsoTpFailure::NotSent);
                    }
                }
                None => {
                    if now >= next_at {
                        self.send_consecutive(now);
                    }
                }
            },
        }
    }

    /// Emit the consecutive frame(s) now due. With STmin = 0 the whole
    /// block is queued at once; the bus owner is responsible for pacing
    /// against its hardware transmit queue.
    fn send_consecutive(&mut self, now: u64) {
        let dl = usize::from(self.p.tx_dl);
        loop {
            let remaining = self.tx_data.len() - self.tx_offset;
            let n = remaining.min(dl - 1);
            let mut pdu = Vec::with_capacity(n + 1);
            pdu.push((PCI_CF << 4) | self.tx_sn);
            pdu.extend_from_slice(&self.tx_data[self.tx_offset..self.tx_offset + n]);
            self.queue_frame(&pdu);
            self.tx_sn = (self.tx_sn + 1) & 0x0F;
            self.tx_offset += n;

            if self.tx_offset == self.tx_data.len() {
                self.tx_data = Vec::new();
                self.tx_state = TxState::Idle;
                self.events.push_back(Event::TxDone);
                return;
            }
            if let Some(left) = &mut self.tx_block_left {
                *left -= 1;
                if *left == 0 {
                    self.tx_state = TxState::WaitFc {
                        deadline: now + u64::from(self.p.n_bs_ms) * NS_PER_MS,
                    };
                    return;
                }
            }
            if self.tx_st_min_ns != 0 {
                self.tx_state = TxState::Sending {
                    next_at: now + self.tx_st_min_ns,
                };
                return;
            }
        }
    }

    fn on_single(&mut self, d: &[u8]) {
        let (len, offset) = if d[0] & 0x0F != 0 {
            // The short form is only valid in frames of up to 8 bytes.
            if d.len() > 8 {
                return;
            }
            (usize::from(d[0] & 0x0F), 1)
        } else {
            // Escape form, CAN FD frames longer than 8 bytes only.
            if d.len() <= 8 {
                return;
            }
            (usize::from(d[1]), 2)
        };
        if len == 0 || offset + len > d.len() || (offset == 2 && len < 8) {
            return;
        }
        if matches!(self.rx_state, RxState::Receiving { .. }) {
            self.fail_rx(IsoTpFailure::Aborted);
        }
        self.events
            .push_back(Event::Received(d[offset..offset + len].to_vec()));
    }

    fn on_first(&mut self, d: &[u8], now: u64) {
        if d.len() < 8 {
            return;
        }
        let mut total = (usize::from(d[0] & 0x0F) << 8) | usize::from(d[1]);
        let mut offset = 2;
        if total == 0 {
            total = u32::from_be_bytes([d[2], d[3], d[4], d[5]]) as usize;
            offset = 6;
            if total <= 4095 {
                return;
            }
        }
        // A first frame must announce more than fits in the frame itself.
        if total <= d.len() - offset {
            return;
        }
        if matches!(self.rx_state, RxState::Receiving { .. }) {
            self.fail_rx(IsoTpFailure::Aborted);
        }
        if total > self.p.max_rx_len as usize {
            self.queue_flow_control(FC_OVERFLOW);
            return;
        }
        self.rx_buf = Vec::with_capacity(total.min(64 * 1024));
        self.rx_buf.extend_from_slice(&d[offset..]);
        self.rx_total = total;
        self.rx_sn = 1;
        self.rx_block_count = 0;
        self.rx_state = RxState::Receiving {
            deadline: now + u64::from(self.p.n_cr_ms) * NS_PER_MS,
        };
        self.queue_flow_control(FC_CTS);
    }

    fn on_consecutive(&mut self, d: &[u8], now: u64) {
        if !matches!(self.rx_state, RxState::Receiving { .. }) {
            return;
        }
        if d[0] & 0x0F != self.rx_sn {
            self.fail_rx(IsoTpFailure::Sequence);
            return;
        }
        self.rx_sn = (self.rx_sn + 1) & 0x0F;
        let remaining = self.rx_total - self.rx_buf.len();
        let n = remaining.min(d.len() - 1);
        self.rx_buf.extend_from_slice(&d[1..1 + n]);

        if self.rx_buf.len() == self.rx_total {
            self.rx_state = RxState::Idle;
            let msg = std::mem::take(&mut self.rx_buf);
            self.events.push_back(Event::Received(msg));
            return;
        }
        if n == 0 {
            // An empty consecutive frame makes no progress; treat as garbage.
            self.fail_rx(IsoTpFailure::Sequence);
            return;
        }
        self.rx_state = RxState::Receiving {
            deadline: now + u64::from(self.p.n_cr_ms) * NS_PER_MS,
        };
        if self.p.block_size != 0 {
            self.rx_block_count += 1;
            if self.rx_block_count == self.p.block_size {
                self.rx_block_count = 0;
                self.queue_flow_control(FC_CTS);
            }
        }
    }

    fn on_flow_control(&mut self, d: &[u8], now: u64) {
        if !matches!(self.tx_state, TxState::WaitFc { .. }) {
            return;
        }
        if d.len() < 3 {
            self.fail_tx(IsoTpFailure::InvalidFlowControl);
            return;
        }
        match d[0] & 0x0F {
            FC_CTS => {
                self.tx_block_left = (d[1] != 0).then_some(d[1]);
                self.tx_st_min_ns = st_min_to_ns(d[2]);
                self.tx_wait_frames = 0;
                self.tx_state = TxState::Sending { next_at: now };
            }
            FC_WAIT => {
                self.tx_wait_frames += 1;
                if self.tx_wait_frames > MAX_WAIT_FRAMES {
                    self.fail_tx(IsoTpFailure::WaitLimit);
                } else {
                    self.tx_state = TxState::WaitFc {
                        deadline: now + u64::from(self.p.n_bs_ms) * NS_PER_MS,
                    };
                }
            }
            FC_OVERFLOW => self.fail_tx(IsoTpFailure::Overflow),
            _ => self.fail_tx(IsoTpFailure::InvalidFlowControl),
        }
    }

    fn fail_tx(&mut self, reason: IsoTpFailure) {
        self.tx_state = TxState::Idle;
        self.tx_unconfirmed = None;
        self.tx_data = Vec::new();
        self.events.push_back(Event::TxFailed(reason));
    }

    fn fail_rx(&mut self, reason: IsoTpFailure) {
        self.rx_state = RxState::Idle;
        self.rx_buf = Vec::new();
        self.events.push_back(Event::RxFailed(reason));
    }

    fn queue_flow_control(&mut self, status: u8) {
        let pdu = [(PCI_FC << 4) | status, self.p.block_size, self.p.st_min];
        self.queue_frame(&pdu);
    }

    /// Wrap a protocol data unit in a CAN frame, applying padding.
    fn queue_frame(&mut self, pdu: &[u8]) {
        let mut buf = [self.p.padding.unwrap_or(FD_FILL); 64];
        buf[..pdu.len()].copy_from_slice(pdu);
        let mut len = pdu.len();
        if len < 8 && self.p.padding.is_some() {
            len = 8;
        }
        if len > 8 {
            len = fd_len_round_up(len).expect("PDU never exceeds tx_dl");
        }
        let mut flags = 0;
        if self.p.fd {
            flags = CANFD_FDF | if self.p.brs { CANFD_BRS } else { 0 };
        }
        let frame = Frame::from_raw(self.p.tx_id, flags, &buf[..len])
            .expect("validated params always produce a valid frame");
        self.out.push_back(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = NS_PER_MS;

    fn pair(configure: impl Fn(&mut IsoTpParams)) -> (IsoTp, IsoTp) {
        let mut a = IsoTpParams::new(0x7E0, 0x7E8);
        let mut b = IsoTpParams::new(0x7E8, 0x7E0);
        configure(&mut a);
        configure(&mut b);
        a.validate().unwrap();
        b.validate().unwrap();
        (IsoTp::new(a), IsoTp::new(b))
    }

    /// What happened on the simulated bus.
    #[derive(Default)]
    struct Trace {
        /// (time, frame) in transmission order, from either side.
        frames: Vec<(u64, Frame)>,
        a_events: Vec<Event>,
        b_events: Vec<Event>,
        end: u64,
    }

    /// Shuttle frames between two endpoints, jumping the clock to the next
    /// timer whenever the bus goes quiet, until neither has work left.
    fn run(a: &mut IsoTp, b: &mut IsoTp, start: u64, drop_nth_from_a: Option<usize>) -> Trace {
        let mut t = Trace::default();
        let mut now = start;
        let mut sent_by_a = 0;
        loop {
            let mut progressed = false;
            while let Some(f) = a.poll_transmit() {
                progressed = true;
                t.frames.push((now, f));
                sent_by_a += 1;
                if drop_nth_from_a != Some(sent_by_a) {
                    b.on_frame(&f, now);
                }
            }
            while let Some(f) = b.poll_transmit() {
                progressed = true;
                t.frames.push((now, f));
                a.on_frame(&f, now);
            }
            while let Some(e) = a.poll_event() {
                t.a_events.push(e);
            }
            while let Some(e) = b.poll_event() {
                t.b_events.push(e);
            }
            if progressed {
                continue;
            }
            let next = match (a.next_deadline(), b.next_deadline()) {
                (Some(x), Some(y)) => x.min(y),
                (x, y) => match x.or(y) {
                    Some(d) => d,
                    None => break,
                },
            };
            now = now.max(next);
            a.poll(now);
            b.poll(now);
        }
        t.end = now;
        t
    }

    fn payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn single_frame() {
        let (mut a, mut b) = pair(|_| {});
        a.send(vec![0x22, 0xF1, 0x90], 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert_eq!(t.frames.len(), 1);
        assert_eq!(t.frames[0].1.data(), &[0x03, 0x22, 0xF1, 0x90]);
        assert_eq!(t.a_events, vec![Event::TxDone]);
        assert_eq!(t.b_events, vec![Event::Received(vec![0x22, 0xF1, 0x90])]);
    }

    #[test]
    fn padding_fills_every_frame() {
        let (mut a, mut b) = pair(|p| p.padding = Some(0xAA));
        a.send(payload(20), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert!(t.frames.iter().all(|(_, f)| f.len() == 8));
        // Flow control frame from b: CTS, BS 0, STmin 0, then padding.
        assert_eq!(
            t.frames[1].1.data(),
            &[0x30, 0, 0, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]
        );
        assert_eq!(t.b_events, vec![Event::Received(payload(20))]);
    }

    #[test]
    fn multi_frame_classic() {
        let (mut a, mut b) = pair(|_| {});
        let msg = payload(100);
        a.send(msg.clone(), 0).unwrap();
        assert!(a.is_sending());
        let t = run(&mut a, &mut b, 0, None);
        // FF carries 6 bytes, then ceil(94 / 7) = 14 CFs, plus one FC.
        assert_eq!(t.frames.len(), 1 + 1 + 14);
        assert_eq!(&t.frames[0].1.data()[..2], &[0x10, 100]);
        assert_eq!(t.frames[2].1.data()[0], 0x21);
        assert_eq!(t.a_events, vec![Event::TxDone]);
        assert_eq!(t.b_events, vec![Event::Received(msg)]);
        assert!(!a.is_sending());
    }

    #[test]
    fn sequence_number_wraps() {
        let (mut a, mut b) = pair(|_| {});
        let msg = payload(6 + 7 * 20);
        a.send(msg.clone(), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        let cfs: Vec<u8> = t.frames[2..].iter().map(|(_, f)| f.data()[0]).collect();
        assert_eq!(cfs[14], 0x2F);
        assert_eq!(cfs[15], 0x20);
        assert_eq!(t.b_events, vec![Event::Received(msg)]);
    }

    #[test]
    fn block_size_and_st_min_are_honoured() {
        let (mut a, mut b) = pair(|p| {
            p.block_size = 4;
            p.st_min = 5;
        });
        let msg = payload(6 + 7 * 10);
        a.send(msg.clone(), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert_eq!(t.b_events, vec![Event::Received(msg)]);

        let fcs = t.frames.iter().filter(|(_, f)| f.can_id() == 0x7E8).count();
        // One after the FF, then after CF 4 and CF 8 (not after the last).
        assert_eq!(fcs, 3);

        // Within a block, consecutive frames are at least STmin apart.
        let cf_times: Vec<u64> = t
            .frames
            .iter()
            .filter(|(_, f)| f.can_id() == 0x7E0 && f.data()[0] >> 4 == PCI_CF)
            .map(|(at, _)| *at)
            .collect();
        assert_eq!(cf_times.len(), 10);
        for block in cf_times.chunks(4) {
            for w in block.windows(2) {
                assert_eq!(w[1] - w[0], 5 * MS);
            }
        }
    }

    #[test]
    fn a_pending_frame_holds_the_next_until_it_is_confirmed() {
        let (mut a, mut b) = pair(|p| p.st_min = 5);
        a.send(payload(40), 0).unwrap();
        b.on_frame(&a.poll_transmit().unwrap(), 0);
        a.on_frame(&b.poll_transmit().unwrap(), 0);

        // The bus will say when the first consecutive frame went out.
        let cf = a.poll_transmit().unwrap();
        a.frame_pending(&cf, 0);
        assert_eq!(a.next_deadline(), Some(1000 * MS));
        // STmin has long passed, but the frame is not on the bus yet.
        a.poll(20 * MS);
        assert!(a.poll_transmit().is_none());
        // A report for some other frame changes nothing.
        let other = Frame::from_raw(cf.can_id(), 0, &[0x2F, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        a.frame_transmitted(&other, 21 * MS);
        a.poll(30 * MS);
        assert!(a.poll_transmit().is_none());
        // It went out at 22 ms: the next is due STmin after that.
        a.frame_transmitted(&cf, 22 * MS);
        assert_eq!(a.next_deadline(), Some(27 * MS));
        a.poll(26 * MS);
        assert!(a.poll_transmit().is_none());
        a.poll(27 * MS);
        let cf = a.poll_transmit().unwrap();
        assert_eq!(cf.data()[0], 0x22);

        // A frame that never gets out fails the message after N_As.
        a.frame_pending(&cf, 27 * MS);
        a.poll(1026 * MS);
        assert!(a.is_sending());
        a.poll(1027 * MS);
        assert_eq!(a.poll_event(), Some(Event::TxFailed(IsoTpFailure::NotSent)));
        assert!(!a.is_sending());
        assert_eq!(a.next_deadline(), None);
    }

    #[test]
    fn a_send_can_be_given_up() {
        let (mut a, _) = pair(|_| {});
        a.abort_send();
        assert_eq!(a.poll_event(), None);
        a.send(payload(40), 0).unwrap();
        a.abort_send();
        assert_eq!(a.poll_event(), Some(Event::TxFailed(IsoTpFailure::NotSent)));
        assert!(!a.is_sending());
    }

    #[test]
    fn st_min_counts_from_the_actual_transmission() {
        let (mut a, mut b) = pair(|p| p.st_min = 5);
        a.send(payload(40), 0).unwrap();
        b.on_frame(&a.poll_transmit().unwrap(), 0);
        a.on_frame(&b.poll_transmit().unwrap(), 0);

        // The first consecutive frame is produced at t = 0 but only leaves
        // 2 ms later.
        let cf = a.poll_transmit().unwrap();
        assert_eq!(a.next_deadline(), Some(5 * MS));
        a.frame_transmitted(&cf, 2 * MS);
        assert_eq!(a.next_deadline(), Some(7 * MS));
        a.poll(5 * MS);
        assert!(a.poll_transmit().is_none());
        a.poll(7 * MS);
        let cf = a.poll_transmit().unwrap();
        assert_eq!(cf.data()[0], 0x22);

        // Frames that are not consecutive frames do not move the timer.
        let before = a.next_deadline();
        let fc = Frame::from_raw(0x7E0, 0, &[0x30, 0, 0]).unwrap();
        a.frame_transmitted(&fc, 100 * MS);
        assert_eq!(a.next_deadline(), before);
    }

    #[test]
    fn sub_millisecond_st_min() {
        assert_eq!(st_min_to_ns(0xF1), 100_000);
        assert_eq!(st_min_to_ns(0xF9), 900_000);
        assert_eq!(st_min_to_ns(0x7F), 127 * MS);
        assert_eq!(st_min_to_ns(0x80), 127 * MS);
        assert_eq!(st_min_to_ns(0xFA), 127 * MS);
    }

    #[test]
    fn can_fd_single_frame_with_escape() {
        let (mut a, mut b) = pair(|p| {
            p.fd = true;
            p.brs = true;
            p.tx_dl = 64;
        });
        let msg = payload(30);
        a.send(msg.clone(), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert_eq!(t.frames.len(), 1);
        let f = t.frames[0].1;
        assert!(f.is_fd() && f.is_brs());
        assert_eq!(f.len(), 32);
        assert_eq!(&f.data()[..2], &[0x00, 30]);
        assert_eq!(t.b_events, vec![Event::Received(msg)]);

        // Up to 7 bytes still uses the short form in an 8-byte-or-less frame.
        a.send(payload(7), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert_eq!(t.frames[0].1.len(), 8);
        assert_eq!(t.frames[0].1.data()[0], 0x07);
        assert_eq!(t.b_events, vec![Event::Received(payload(7))]);

        // 62 bytes is the largest FD single frame.
        a.send(payload(62), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert_eq!(t.frames.len(), 1);
        assert_eq!(t.b_events, vec![Event::Received(payload(62))]);
    }

    #[test]
    fn can_fd_multi_frame() {
        let (mut a, mut b) = pair(|p| {
            p.fd = true;
            p.tx_dl = 64;
        });
        let msg = payload(1000);
        a.send(msg.clone(), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        // FF carries 62, each CF 63: ceil(938 / 63) = 15 CFs, plus one FC.
        assert_eq!(t.frames.len(), 1 + 1 + 15);
        // The last CF holds 56 bytes + PCI = 57, rounded up to 64.
        assert_eq!(t.frames.last().unwrap().1.len(), 64);
        assert_eq!(t.b_events, vec![Event::Received(msg)]);
    }

    #[test]
    fn long_message_uses_32_bit_length() {
        let (mut a, mut b) = pair(|_| {});
        let msg = payload(5000);
        a.send(msg.clone(), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert_eq!(
            &t.frames[0].1.data()[..6],
            &[0x10, 0x00, 0x00, 0x00, 0x13, 0x88]
        );
        assert_eq!(t.b_events, vec![Event::Received(msg)]);
    }

    #[test]
    fn simultaneous_transfers_in_both_directions() {
        let (mut a, mut b) = pair(|_| {});
        a.send(payload(200), 0).unwrap();
        b.send(payload(300), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert!(t.a_events.contains(&Event::TxDone));
        assert!(t.a_events.contains(&Event::Received(payload(300))));
        assert!(t.b_events.contains(&Event::TxDone));
        assert!(t.b_events.contains(&Event::Received(payload(200))));
    }

    #[test]
    fn times_out_without_flow_control() {
        let (mut a, _) = pair(|_| {});
        // A peer listening on unrelated ids never answers.
        let mut deaf = IsoTp::new(IsoTpParams::new(0x100, 0x101));
        a.send(payload(50), 10 * MS).unwrap();
        assert_eq!(a.send(payload(5), 10 * MS), Err(IsoTpFailure::Busy));
        let t = run(&mut a, &mut deaf, 10 * MS, None);
        assert_eq!(t.a_events, vec![Event::TxFailed(IsoTpFailure::Timeout)]);
        assert_eq!(t.end, 1010 * MS);
        // The channel is usable again afterwards.
        assert!(a.send(payload(5), t.end).is_ok());
    }

    #[test]
    fn lost_consecutive_frame_is_detected() {
        let (mut a, mut b) = pair(|_| {});
        a.send(payload(100), 0).unwrap();
        // Frame 1 from a is the FF; drop its third frame (the second CF).
        let t = run(&mut a, &mut b, 0, Some(3));
        assert_eq!(t.a_events, vec![Event::TxDone]);
        assert_eq!(t.b_events, vec![Event::RxFailed(IsoTpFailure::Sequence)]);
    }

    #[test]
    fn receiver_times_out_when_sender_stops() {
        let (_, mut b) = pair(|_| {});
        let ff = Frame::from_raw(0x7E0, 0, &[0x10, 20, 1, 2, 3, 4, 5, 6]).unwrap();
        b.on_frame(&ff, 0);
        assert_eq!(b.next_deadline(), Some(1000 * MS));
        b.poll(999 * MS);
        assert_eq!(b.poll_event(), None);
        b.poll(1000 * MS);
        assert_eq!(b.poll_event(), Some(Event::RxFailed(IsoTpFailure::Timeout)));
        assert_eq!(b.next_deadline(), None);
    }

    #[test]
    fn oversized_message_is_refused_with_overflow() {
        let (mut a, mut b) = pair(|p| p.max_rx_len = 64);
        a.send(payload(65), 0).unwrap();
        let t = run(&mut a, &mut b, 0, None);
        assert_eq!(t.frames[1].1.data()[0], 0x32);
        assert_eq!(t.a_events, vec![Event::TxFailed(IsoTpFailure::Overflow)]);
        assert!(t.b_events.is_empty());
    }

    #[test]
    fn flow_control_wait_extends_then_gives_up() {
        let (mut a, _) = pair(|_| {});
        a.send(payload(50), 0).unwrap();
        a.poll_transmit().unwrap();
        let wait = Frame::from_raw(0x7E8, 0, &[0x31, 0, 0]).unwrap();
        a.on_frame(&wait, 900 * MS);
        assert_eq!(a.next_deadline(), Some(1900 * MS));
        for i in 0..MAX_WAIT_FRAMES {
            assert_eq!(a.poll_event(), None);
            a.on_frame(&wait, (901 + u64::from(i)) * MS);
        }
        assert_eq!(
            a.poll_event(),
            Some(Event::TxFailed(IsoTpFailure::WaitLimit))
        );
    }

    #[test]
    fn rejects_empty_message_and_ignores_malformed_frames() {
        let (mut a, mut b) = pair(|_| {});
        assert_eq!(a.send(vec![], 0), Err(IsoTpFailure::InvalidLength));

        for bytes in [
            &[0x00u8, 1, 2][..],              // SF with length 0
            &[0x05, 1, 2][..],                // SF longer than the frame
            &[0x10, 5, 1, 2, 3, 4, 5, 6][..], // FF that would fit a SF
            &[0x21, 1, 2][..],                // CF with no transfer in progress
            &[0x30, 0, 0][..],                // FC with no transfer in progress
            &[0x40, 0, 0][..],                // unknown PCI
        ] {
            b.on_frame(&Frame::from_raw(0x7E0, 0, bytes).unwrap(), 0);
        }
        // Wrong id and remote frames are ignored too.
        b.on_frame(&Frame::from_raw(0x7E1, 0, &[0x01, 0xAA]).unwrap(), 0);
        assert_eq!(b.poll_event(), None);
        assert!(b.poll_transmit().is_none());
        assert_eq!(b.next_deadline(), None);
    }

    #[test]
    fn new_message_aborts_reception_in_progress() {
        let (_, mut b) = pair(|_| {});
        let ff = Frame::from_raw(0x7E0, 0, &[0x10, 20, 1, 2, 3, 4, 5, 6]).unwrap();
        b.on_frame(&ff, 0);
        let sf = Frame::from_raw(0x7E0, 0, &[0x02, 0xAB, 0xCD]).unwrap();
        b.on_frame(&sf, MS);
        assert_eq!(b.poll_event(), Some(Event::RxFailed(IsoTpFailure::Aborted)));
        assert_eq!(b.poll_event(), Some(Event::Received(vec![0xAB, 0xCD])));
    }
}
