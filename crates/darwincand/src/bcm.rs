// SPDX-License-Identifier: GPL-2.0-only

//! The broadcast manager: jobs that send frames cyclically and jobs that
//! watch received frames, for one client. No I/O and no clock: the caller
//! passes the time in and takes frames to send and reports to deliver out,
//! so the timing can be tested to the nanosecond.
//!
//! The behaviour follows the Linux `CAN_BCM` protocol as its documentation
//! and `<linux/can/bcm.h>` describe it, operation by operation and flag by
//! flag, so that a program written for it sees the same thing here.
//!
//! A transmit job sends its frames in turn: `count` times at `ival1`, then
//! at `ival2` for as long as it exists. A receive job is one of:
//!
//! - by identifier only (`RX_FILTER_ID`, or no frames given): every frame
//!   with the identifier is reported;
//! - one frame given: its data is a mask, and a frame is reported when the
//!   bits under the mask differ from the last frame's;
//! - several frames given: the first is a mask that picks out a
//!   multiplexer in the first eight bytes, the others are masks each
//!   holding one multiplexer value, and a frame is compared with the last
//!   frame of the same multiplexer value;
//! - `RX_RTR_FRAME`: a remote frame is answered with the frame given.
//!
//! `ival1` is a receive job's timeout and `ival2` the least time between
//! two reports.

use std::collections::VecDeque;

use darwincan_core::bcm::{flag, op, BcmMessage, MAX_FRAMES};
use darwincan_core::{Frame, CAN_RTR_FLAG};

/// Why a request was refused.
pub(crate) type Refusal = &'static str;

struct TxJob {
    can_id: u32,
    fd: bool,
    flags: u32,
    count: u32,
    ival1_ns: u64,
    ival2_ns: u64,
    frames: Vec<Frame>,
    /// The most frames the job has held: it may not grow beyond that.
    capacity: usize,
    /// Which frame goes out next.
    next: usize,
    /// When the timer fires, if it is running.
    deadline: Option<u64>,
}

/// What a receive job remembers of the last frame it compared against.
#[derive(Clone, Copy, Default)]
struct Last {
    frame: Option<(Frame, u64)>,
    /// Changed, and not reported yet because reports are being spaced out.
    held_back: bool,
}

struct RxJob {
    can_id: u32,
    fd: bool,
    flags: u32,
    count: u32,
    ival1_ns: u64,
    ival2_ns: u64,
    /// The frames given at setup: masks, or the answer to a remote frame.
    masks: Vec<Frame>,
    capacity: usize,
    /// One entry per mask; a single one when there are no masks.
    last: Vec<Last>,
    timeout_at: Option<u64>,
    /// When the changes held back are to be reported.
    release_at: Option<u64>,
    last_report: Option<u64>,
}

/// The most transmit jobs, and the most receive jobs, one client may
/// have. Linux sets no limit; here every job is looked at whenever a
/// timer is due and for every frame received, on a real-time thread, so
/// an endless list would be felt by the whole interface.
const MAX_JOBS: usize = 4096;

#[derive(Default)]
pub(crate) struct Bcm {
    tx: Vec<TxJob>,
    rx: Vec<RxJob>,
    transmit: VecDeque<Frame>,
    reports: VecDeque<(u64, BcmMessage)>,
}

fn us_to_ns(us: u64) -> u64 {
    us.saturating_mul(1_000)
}

/// Eight data bytes of a frame from `offset`, as one number.
fn word(frame: &Frame, offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    let data = frame.data();
    if offset < data.len() {
        let end = data.len().min(offset + 8);
        bytes[..end - offset].copy_from_slice(&data[offset..end]);
    }
    u64::from_le_bytes(bytes)
}

/// `frame` with another identifier (and that identifier's flag bits).
fn with_id(frame: &Frame, can_id: u32) -> Result<Frame, Refusal> {
    let zeros = [0u8; 8];
    let data = if can_id & CAN_RTR_FLAG != 0 {
        zeros
            .get(..frame.len())
            .ok_or("a remote frame is a classic frame")?
    } else {
        frame.data()
    };
    Frame::from_raw(can_id, frame.flags(), data)
        .map_err(|_| "the identifier does not fit the job's frames")
}

/// The first time after `now` that lies a whole number of `interval`s
/// after `from`: a timer that is late does not send a burst to catch up.
fn forward(from: u64, interval: u64, now: u64) -> u64 {
    let next = from.saturating_add(interval);
    if next > now {
        return next;
    }
    let missed = (now - from) / interval;
    from.saturating_add(interval.saturating_mul(missed + 1))
}

impl TxJob {
    fn send(&mut self, transmit: &mut VecDeque<Frame>) {
        transmit.push_back(self.frames[self.next]);
        self.next = (self.next + 1) % self.frames.len();
    }

    fn status(&self, opcode: u32, with_frames: bool) -> BcmMessage {
        BcmMessage {
            opcode,
            flags: self.flags,
            count: self.count,
            ival1_us: self.ival1_ns / 1_000,
            ival2_us: self.ival2_ns / 1_000,
            can_id: self.can_id,
            frames: if with_frames {
                self.frames.clone()
            } else {
                Vec::new()
            },
        }
    }

    /// The interval the timer runs at now, if it runs.
    fn interval(&self) -> Option<u64> {
        if self.ival1_ns > 0 && self.count > 0 {
            Some(self.ival1_ns)
        } else if self.ival2_ns > 0 {
            Some(self.ival2_ns)
        } else {
            None
        }
    }
}

impl RxJob {
    fn message(&self, opcode: u32, frames: Vec<Frame>) -> BcmMessage {
        BcmMessage {
            opcode,
            flags: self.flags,
            count: self.count,
            ival1_us: self.ival1_ns / 1_000,
            ival2_us: self.ival2_ns / 1_000,
            can_id: self.can_id,
            frames,
        }
    }

    fn report(&mut self, index: usize, reports: &mut VecDeque<(u64, BcmMessage)>) {
        self.last[index].held_back = false;
        if let Some((frame, at)) = self.last[index].frame {
            reports.push_back((at, self.message(op::RX_CHANGED, vec![frame])));
        }
    }

    /// Report every change held back; says whether there was one.
    fn release(&mut self, reports: &mut VecDeque<(u64, BcmMessage)>) -> bool {
        let mut any = false;
        for index in 0..self.last.len() {
            if self.last[index].held_back {
                self.report(index, reports);
                any = true;
            }
        }
        any
    }

    /// A frame that counts as new for entry `index`: remember it, and
    /// report it now or when reports may next be made.
    fn changed(
        &mut self,
        index: usize,
        frame: &Frame,
        at: u64,
        now: u64,
        reports: &mut VecDeque<(u64, BcmMessage)>,
    ) {
        self.last[index] = Last {
            frame: Some((*frame, at)),
            held_back: true,
        };
        if self.ival2_ns == 0 {
            self.report(index, reports);
            return;
        }
        if self.release_at.is_some() {
            return;
        }
        match self.last_report {
            Some(last) if now.saturating_sub(last) < self.ival2_ns => {
                self.release_at = Some(last + self.ival2_ns);
            }
            _ => {
                self.report(index, reports);
                self.last_report = Some(now);
            }
        }
    }

    /// Compare a frame with the last one under mask `index`.
    fn compare(
        &mut self,
        index: usize,
        frame: &Frame,
        at: u64,
        now: u64,
        reports: &mut VecDeque<(u64, BcmMessage)>,
    ) {
        let differs = match &self.last[index].frame {
            None => true,
            Some((last, _)) => {
                let mask = &self.masks[index];
                (0..frame.len())
                    .step_by(8)
                    .any(|o| word(mask, o) & word(frame, o) != word(mask, o) & word(last, o))
                    || (self.flags & flag::RX_CHECK_DLC != 0 && frame.len() != last.len())
            }
        };
        if differs {
            self.changed(index, frame, at, now, reports);
        }
    }
}

impl Bcm {
    pub(crate) fn new() -> Bcm {
        Bcm::default()
    }

    /// Carry out a request from the client. Frames it makes go out
    /// through [`poll_transmit`](Self::poll_transmit) and what it reports
    /// through [`poll_report`](Self::poll_report).
    pub(crate) fn request(&mut self, msg: &BcmMessage, now: u64) -> Result<(), Refusal> {
        let fd = msg.flags & flag::CAN_FD_FRAME != 0;
        if msg.frames.iter().any(|f| f.is_fd() != fd) {
            return Err("the frames must be CAN FD frames exactly when CAN_FD_FRAME is set");
        }
        if msg.frames.iter().any(Frame::is_error) {
            return Err("an error frame cannot be part of a job");
        }
        match msg.opcode {
            op::TX_SETUP => self.tx_setup(msg, fd, now),
            op::TX_DELETE => {
                let before = self.tx.len();
                self.tx.retain(|j| !(j.can_id == msg.can_id && j.fd == fd));
                if self.tx.len() == before {
                    return Err("no such transmit job");
                }
                Ok(())
            }
            op::TX_READ => {
                let job = self
                    .tx
                    .iter()
                    .find(|j| j.can_id == msg.can_id && j.fd == fd)
                    .ok_or("no such transmit job")?;
                self.reports
                    .push_back((now, job.status(op::TX_STATUS, true)));
                Ok(())
            }
            op::TX_SEND => match msg.frames.as_slice() {
                [frame] => {
                    self.transmit.push_back(*frame);
                    Ok(())
                }
                _ => Err("TX_SEND takes exactly one frame"),
            },
            op::RX_SETUP => self.rx_setup(msg, fd, now),
            op::RX_DELETE => {
                let before = self.rx.len();
                self.rx.retain(|j| !(j.can_id == msg.can_id && j.fd == fd));
                if self.rx.len() == before {
                    return Err("no such receive job");
                }
                Ok(())
            }
            op::RX_READ => {
                let job = self
                    .rx
                    .iter()
                    .find(|j| j.can_id == msg.can_id && j.fd == fd)
                    .ok_or("no such receive job")?;
                self.reports
                    .push_back((now, job.message(op::RX_STATUS, job.masks.clone())));
                Ok(())
            }
            _ => Err("unknown broadcast manager operation"),
        }
    }

    fn tx_setup(&mut self, msg: &BcmMessage, fd: bool, now: u64) -> Result<(), Refusal> {
        if msg.frames.is_empty() || msg.frames.len() > MAX_FRAMES {
            return Err("a transmit job holds 1 to 256 frames");
        }
        let frames = if msg.flags & flag::TX_CP_CAN_ID != 0 {
            msg.frames
                .iter()
                .map(|f| with_id(f, msg.can_id))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            msg.frames.clone()
        };
        let job = match self
            .tx
            .iter_mut()
            .find(|j| j.can_id == msg.can_id && j.fd == fd)
        {
            Some(job) => {
                if frames.len() > job.capacity {
                    return Err("more frames than the job was created with");
                }
                if frames.len() != job.frames.len() {
                    job.next = 0;
                }
                job.frames = frames;
                job.flags = msg.flags;
                job
            }
            None => {
                if self.tx.len() >= MAX_JOBS {
                    return Err("too many transmit jobs");
                }
                self.tx.push(TxJob {
                    can_id: msg.can_id,
                    fd,
                    flags: msg.flags,
                    count: 0,
                    ival1_ns: 0,
                    ival2_ns: 0,
                    capacity: frames.len(),
                    frames,
                    next: 0,
                    deadline: None,
                });
                self.tx.last_mut().expect("just pushed")
            }
        };
        if job.flags & flag::TX_RESET_MULTI_IDX != 0 {
            job.next = 0;
        }
        if job.flags & flag::SETTIMER != 0 {
            job.count = msg.count;
            job.ival1_ns = us_to_ns(msg.ival1_us);
            job.ival2_ns = us_to_ns(msg.ival2_us);
            if job.ival1_ns == 0 && job.ival2_ns == 0 {
                job.deadline = None;
            }
        }
        if job.flags & flag::STARTTIMER != 0 {
            // Starting the timer sends the first frame at once.
            job.deadline = None;
            job.flags |= flag::TX_ANNOUNCE;
        }
        if job.flags & flag::TX_ANNOUNCE != 0 {
            job.send(&mut self.transmit);
            job.count = job.count.saturating_sub(1);
        }
        if job.flags & flag::STARTTIMER != 0 {
            job.deadline = job.interval().map(|i| now.saturating_add(i));
        }
        Ok(())
    }

    fn rx_setup(&mut self, msg: &BcmMessage, fd: bool, now: u64) -> Result<(), Refusal> {
        let by_id = msg.flags & flag::RX_FILTER_ID != 0 || msg.frames.is_empty();
        let (flags, mut masks) = if by_id {
            (msg.flags | flag::RX_FILTER_ID, Vec::new())
        } else {
            (msg.flags, msg.frames.clone())
        };
        if masks.len() > MAX_FRAMES + 1 {
            return Err("a receive job holds at most 257 frames");
        }
        let answers_remote = flags & flag::RX_RTR_FRAME != 0;
        if answers_remote {
            if masks.len() != 1 || msg.can_id & CAN_RTR_FLAG == 0 {
                return Err("RX_RTR_FRAME takes one frame and an identifier with CAN_RTR_FLAG set");
            }
            // The answer is a data frame even if it was given with the
            // remote identifier, or it would answer itself for ever.
            if flags & flag::TX_CP_CAN_ID != 0 || masks[0].can_id() == msg.can_id {
                masks[0] = with_id(&masks[0], msg.can_id & !CAN_RTR_FLAG)?;
            }
        }
        let entries = masks.len().max(1);
        let job = match self
            .rx
            .iter_mut()
            .find(|j| j.can_id == msg.can_id && j.fd == fd)
        {
            Some(job) => {
                if masks.len() > job.capacity {
                    return Err("more frames than the job was created with");
                }
                job.masks = masks;
                job.last = vec![Last::default(); entries];
                job.flags = flags;
                job
            }
            None => {
                if self.rx.len() >= MAX_JOBS {
                    return Err("too many receive jobs");
                }
                self.rx.push(RxJob {
                    can_id: msg.can_id,
                    fd,
                    flags,
                    count: 0,
                    ival1_ns: 0,
                    ival2_ns: 0,
                    capacity: masks.len(),
                    masks,
                    last: vec![Last::default(); entries],
                    timeout_at: None,
                    release_at: None,
                    last_report: None,
                });
                self.rx.last_mut().expect("just pushed")
            }
        };
        if answers_remote {
            job.timeout_at = None;
            job.release_at = None;
            return Ok(());
        }
        if flags & flag::SETTIMER != 0 {
            job.count = msg.count;
            job.ival1_ns = us_to_ns(msg.ival1_us);
            job.ival2_ns = us_to_ns(msg.ival2_us);
            if job.ival1_ns == 0 {
                job.timeout_at = None;
            }
            if job.ival2_ns > 0 {
                job.last_report = None;
                job.release_at = None;
                job.release(&mut self.reports);
            }
        }
        if flags & flag::STARTTIMER != 0 && job.ival1_ns > 0 {
            job.timeout_at = Some(now.saturating_add(job.ival1_ns));
        }
        Ok(())
    }

    /// The identifiers the receive jobs watch, with whether each is for
    /// CAN FD frames: all that [`on_frame`](Self::on_frame) need be given.
    pub(crate) fn watched(&self) -> Vec<(u32, bool)> {
        self.rx.iter().map(|j| (j.can_id, j.fd)).collect()
    }

    /// A frame was on the bus at time `at`.
    pub(crate) fn on_frame(&mut self, frame: &Frame, at: u64, now: u64) {
        for job in &mut self.rx {
            if job.can_id != frame.can_id() || job.fd != frame.is_fd() {
                continue;
            }
            job.timeout_at = None;
            if job.flags & flag::RX_RTR_FRAME != 0 {
                self.transmit.push_back(job.masks[0]);
                continue;
            }
            if job.flags & flag::RX_FILTER_ID != 0 {
                job.changed(0, frame, at, now, &mut self.reports);
            } else if job.masks.len() == 1 {
                job.compare(0, frame, at, now, &mut self.reports);
            } else {
                let selector = word(&job.masks[0], 0);
                let value = selector & word(frame, 0);
                if let Some(index) =
                    (1..job.masks.len()).find(|&i| selector & word(&job.masks[i], 0) == value)
                {
                    job.compare(index, frame, at, now, &mut self.reports);
                }
            }
            if job.flags & flag::RX_NO_AUTOTIMER == 0 && job.ival1_ns > 0 {
                job.timeout_at = Some(now.saturating_add(job.ival1_ns));
            }
        }
    }

    /// When [`poll`](Self::poll) next has something to do.
    pub(crate) fn next_deadline(&self) -> Option<u64> {
        let tx = self.tx.iter().filter_map(|j| j.deadline);
        let rx = self
            .rx
            .iter()
            .flat_map(|j| [j.timeout_at, j.release_at])
            .flatten();
        tx.chain(rx).min()
    }

    /// Let the timers that are due fire.
    pub(crate) fn poll(&mut self, now: u64) {
        for job in &mut self.tx {
            let Some(deadline) = job.deadline.filter(|&d| d <= now) else {
                continue;
            };
            if job.ival1_ns > 0 && job.count > 0 {
                job.count -= 1;
                if job.count == 0 && job.flags & flag::TX_COUNTEVT != 0 {
                    self.reports
                        .push_back((now, job.status(op::TX_EXPIRED, false)));
                }
                job.send(&mut self.transmit);
            } else if job.ival2_ns > 0 {
                job.send(&mut self.transmit);
            }
            job.deadline = job.interval().map(|i| forward(deadline, i, now));
        }
        for job in &mut self.rx {
            if let Some(due) = job.release_at.filter(|&d| d <= now) {
                if job.release(&mut self.reports) {
                    job.release_at = Some(forward(due, job.ival2_ns.max(1), now));
                } else {
                    job.release_at = None;
                    job.last_report = None;
                }
            }
            if job.timeout_at.is_some_and(|d| d <= now) {
                job.timeout_at = None;
                self.reports
                    .push_back((now, job.message(op::RX_TIMEOUT, Vec::new())));
                if job.flags & flag::RX_ANNOUNCE_RESUME != 0 {
                    job.last.fill(Last::default());
                }
            }
        }
    }

    /// The next frame to put on the bus.
    pub(crate) fn poll_transmit(&mut self) -> Option<Frame> {
        self.transmit.pop_front()
    }

    /// The next report for the client, with the time it belongs to.
    pub(crate) fn poll_report(&mut self) -> Option<(u64, BcmMessage)> {
        self.reports.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darwincan_core::Id;

    const MS: u64 = 1_000_000;

    fn frame(id: u16, data: &[u8]) -> Frame {
        Frame::new(Id::Standard(id), data).unwrap()
    }

    fn sent(bcm: &mut Bcm) -> Vec<Frame> {
        std::iter::from_fn(|| bcm.poll_transmit()).collect()
    }

    fn reported(bcm: &mut Bcm) -> Vec<(u64, u32, Vec<Frame>)> {
        std::iter::from_fn(|| bcm.poll_report())
            .map(|(at, m)| (at, m.opcode, m.frames))
            .collect()
    }

    /// Run the timers to `until`, and return when each frame went out.
    fn run(bcm: &mut Bcm, until: u64) -> Vec<(u64, Frame)> {
        let mut out = Vec::new();
        while let Some(deadline) = bcm.next_deadline().filter(|&d| d <= until) {
            bcm.poll(deadline);
            out.extend(sent(bcm).into_iter().map(|f| (deadline, f)));
        }
        out
    }

    #[test]
    fn a_cyclic_job_sends_at_once_and_then_on_the_interval() {
        let mut bcm = Bcm::new();
        let f = frame(0x123, &[1]);
        bcm.request(&BcmMessage::tx_cyclic(f, 10_000), 5 * MS)
            .unwrap();
        assert_eq!(sent(&mut bcm), [f]);
        let times: Vec<u64> = run(&mut bcm, 45 * MS).iter().map(|(t, _)| *t).collect();
        assert_eq!(times, [15 * MS, 25 * MS, 35 * MS, 45 * MS]);
    }

    #[test]
    fn a_late_timer_keeps_its_rhythm_and_sends_no_burst() {
        let mut bcm = Bcm::new();
        bcm.request(&BcmMessage::tx_cyclic(frame(0x123, &[1]), 10_000), 0)
            .unwrap();
        sent(&mut bcm);
        // Woken 37 ms late: one frame, and the next on the old grid.
        bcm.poll(47 * MS);
        assert_eq!(sent(&mut bcm).len(), 1);
        assert_eq!(bcm.next_deadline(), Some(50 * MS));
    }

    #[test]
    fn count_frames_go_at_the_first_interval_then_the_second() {
        let mut bcm = Bcm::new();
        let mut msg = BcmMessage::tx_cyclic(frame(0x123, &[1]), 100_000);
        msg.count = 3;
        msg.ival1_us = 10_000;
        msg.flags |= flag::TX_COUNTEVT;
        bcm.request(&msg, 0).unwrap();
        // The frame sent at once is the first of the three.
        assert_eq!(sent(&mut bcm).len(), 1);
        let times: Vec<u64> = run(&mut bcm, 250 * MS).iter().map(|(t, _)| *t).collect();
        assert_eq!(times, [10 * MS, 20 * MS, 120 * MS, 220 * MS]);
        let reports = reported(&mut bcm);
        assert_eq!(reports.len(), 1);
        assert_eq!((reports[0].0, reports[0].1), (20 * MS, op::TX_EXPIRED));
    }

    #[test]
    fn several_frames_go_out_in_turn() {
        let mut bcm = Bcm::new();
        let frames = vec![frame(0x123, &[1]), frame(0x123, &[2]), frame(0x123, &[3])];
        let mut msg = BcmMessage::tx_cyclic(frames[0], 10_000);
        msg.frames = frames.clone();
        bcm.request(&msg, 0).unwrap();
        let mut all = sent(&mut bcm);
        all.extend(run(&mut bcm, 40 * MS).into_iter().map(|(_, f)| f));
        assert_eq!(all, [frames[0], frames[1], frames[2], frames[0], frames[1]]);

        // Starting again from the first on request.
        msg.flags = flag::TX_RESET_MULTI_IDX | flag::TX_ANNOUNCE;
        bcm.request(&msg, 41 * MS).unwrap();
        assert_eq!(sent(&mut bcm), [frames[0]]);
    }

    #[test]
    fn new_data_does_not_disturb_the_timer() {
        let mut bcm = Bcm::new();
        bcm.request(&BcmMessage::tx_cyclic(frame(0x123, &[1]), 10_000), 0)
            .unwrap();
        sent(&mut bcm);
        bcm.request(&BcmMessage::tx_update(frame(0x123, &[9, 9])), 3 * MS)
            .unwrap();
        assert_eq!(sent(&mut bcm), []);
        assert_eq!(run(&mut bcm, 10 * MS), [(10 * MS, frame(0x123, &[9, 9]))]);
    }

    #[test]
    fn jobs_are_kept_apart_deleted_and_read_back() {
        let mut bcm = Bcm::new();
        bcm.request(&BcmMessage::tx_cyclic(frame(0x100, &[1]), 10_000), 0)
            .unwrap();
        bcm.request(&BcmMessage::tx_cyclic(frame(0x200, &[2]), 4_000), 0)
            .unwrap();
        sent(&mut bcm);
        let ids: Vec<u32> = run(&mut bcm, 10 * MS)
            .iter()
            .map(|(_, f)| f.can_id())
            .collect();
        assert_eq!(ids, [0x200, 0x200, 0x100]);

        bcm.request(
            &BcmMessage {
                opcode: op::TX_READ,
                can_id: 0x200,
                ..BcmMessage::default()
            },
            10 * MS,
        )
        .unwrap();
        let (_, status) = bcm.poll_report().unwrap();
        assert_eq!(status.opcode, op::TX_STATUS);
        assert_eq!(status.ival2_us, 4_000);
        assert_eq!(status.frames, [frame(0x200, &[2])]);

        bcm.request(&BcmMessage::tx_delete(0x200, false), 10 * MS)
            .unwrap();
        assert!(bcm
            .request(&BcmMessage::tx_delete(0x200, false), 10 * MS)
            .is_err());
        // A CAN FD job with the same identifier would be another job.
        assert!(bcm
            .request(&BcmMessage::tx_delete(0x100, true), 10 * MS)
            .is_err());
        let ids: Vec<u32> = run(&mut bcm, 30 * MS)
            .iter()
            .map(|(_, f)| f.can_id())
            .collect();
        assert_eq!(ids, [0x100, 0x100]);
    }

    #[test]
    fn a_single_send_and_what_is_refused() {
        let mut bcm = Bcm::new();
        let f = frame(0x321, &[5]);
        let mut msg = BcmMessage {
            opcode: op::TX_SEND,
            frames: vec![f],
            ..BcmMessage::default()
        };
        bcm.request(&msg, 0).unwrap();
        assert_eq!(sent(&mut bcm), [f]);
        assert_eq!(bcm.next_deadline(), None);

        msg.frames.push(f);
        assert!(bcm.request(&msg, 0).is_err());
        msg.opcode = 99;
        assert!(bcm.request(&msg, 0).is_err());
        // A classic frame in a job declared as CAN FD.
        let mut fd = BcmMessage::tx_cyclic(f, 1_000);
        fd.flags |= flag::CAN_FD_FRAME;
        assert!(bcm.request(&fd, 0).is_err());
        let mut empty = BcmMessage::tx_cyclic(f, 1_000);
        empty.frames.clear();
        assert!(bcm.request(&empty, 0).is_err());
        // A job may shrink and grow back, but not beyond what it was.
        bcm.request(&BcmMessage::tx_cyclic(f, 1_000), 0).unwrap();
        let mut more = BcmMessage::tx_update(f);
        more.frames.push(f);
        assert!(bcm.request(&more, 0).is_err());
    }

    #[test]
    fn the_identifier_can_be_copied_into_the_frames() {
        let mut bcm = Bcm::new();
        let mut msg = BcmMessage::tx_cyclic(frame(0x001, &[7]), 10_000);
        msg.can_id = 0x456;
        msg.flags |= flag::TX_CP_CAN_ID;
        bcm.request(&msg, 0).unwrap();
        assert_eq!(sent(&mut bcm), [frame(0x456, &[7])]);
    }

    #[test]
    fn watching_by_identifier_reports_every_frame_and_silence() {
        let mut bcm = Bcm::new();
        bcm.request(&BcmMessage::rx_watch(0x123, 50_000), 0)
            .unwrap();
        assert_eq!(bcm.watched(), [(0x123, false)]);
        let f = frame(0x123, &[1]);
        bcm.on_frame(&f, 9 * MS, 10 * MS);
        bcm.on_frame(&f, 19 * MS, 20 * MS);
        bcm.on_frame(&frame(0x124, &[1]), 21 * MS, 21 * MS);
        assert_eq!(
            reported(&mut bcm),
            [
                (9 * MS, op::RX_CHANGED, vec![f]),
                (19 * MS, op::RX_CHANGED, vec![f])
            ]
        );
        // The timeout runs from the last frame, and fires once.
        assert_eq!(bcm.next_deadline(), Some(70 * MS));
        bcm.poll(70 * MS);
        assert_eq!(reported(&mut bcm), [(70 * MS, op::RX_TIMEOUT, vec![])]);
        assert_eq!(bcm.next_deadline(), None);
        // The next frame starts it again.
        bcm.on_frame(&f, 100 * MS, 100 * MS);
        assert_eq!(bcm.next_deadline(), Some(150 * MS));
    }

    #[test]
    fn the_timeout_starts_at_setup_when_asked_to() {
        let mut bcm = Bcm::new();
        bcm.request(&BcmMessage::rx_watch(0x123, 50_000), 7 * MS)
            .unwrap();
        assert_eq!(bcm.next_deadline(), Some(57 * MS));

        // Without STARTTIMER it waits for the first frame.
        let mut bcm = Bcm::new();
        let mut msg = BcmMessage::rx_watch(0x123, 50_000);
        msg.flags &= !flag::STARTTIMER;
        bcm.request(&msg, 7 * MS).unwrap();
        assert_eq!(bcm.next_deadline(), None);

        // With RX_NO_AUTOTIMER a frame stops the timer for good.
        let mut bcm = Bcm::new();
        let mut msg = BcmMessage::rx_watch(0x123, 50_000);
        msg.flags |= flag::RX_NO_AUTOTIMER;
        bcm.request(&msg, 0).unwrap();
        bcm.on_frame(&frame(0x123, &[1]), 10 * MS, 10 * MS);
        assert_eq!(bcm.next_deadline(), None);
    }

    fn content_job(mask: &[u8], flags: u32) -> BcmMessage {
        BcmMessage {
            opcode: op::RX_SETUP,
            flags,
            can_id: 0x123,
            frames: vec![frame(0x123, mask)],
            ..BcmMessage::default()
        }
    }

    #[test]
    fn a_mask_selects_the_bits_whose_change_is_reported() {
        let mut bcm = Bcm::new();
        bcm.request(&content_job(&[0xFF, 0x0F, 0, 0], 0), 0)
            .unwrap();
        let mut ops = Vec::new();
        for (i, data) in [
            [1, 0x11, 5, 5], // the first is always reported
            [1, 0x11, 6, 7], // only bytes outside the mask changed
            [1, 0xF1, 6, 7], // only bits outside the mask changed
            [1, 0x12, 6, 7], // a masked bit changed
            [2, 0x12, 6, 7], // a masked byte changed
            [2, 0x12, 6, 7], // the same again
        ]
        .iter()
        .enumerate()
        {
            bcm.on_frame(&frame(0x123, data), i as u64, i as u64);
            ops.push(reported(&mut bcm).len());
        }
        assert_eq!(ops, [1, 0, 0, 1, 1, 0]);
    }

    #[test]
    fn a_change_of_length_counts_only_when_asked_for() {
        let mut bcm = Bcm::new();
        bcm.request(&content_job(&[0xFF], 0), 0).unwrap();
        bcm.on_frame(&frame(0x123, &[1]), 0, 0);
        bcm.on_frame(&frame(0x123, &[1, 2]), 1, 1);
        assert_eq!(reported(&mut bcm).len(), 1);

        let mut bcm = Bcm::new();
        bcm.request(&content_job(&[0xFF], flag::RX_CHECK_DLC), 0)
            .unwrap();
        bcm.on_frame(&frame(0x123, &[1]), 0, 0);
        bcm.on_frame(&frame(0x123, &[1, 2]), 1, 1);
        assert_eq!(reported(&mut bcm).len(), 2);
    }

    #[test]
    fn reports_are_spaced_out_and_the_latest_change_is_kept() {
        let mut bcm = Bcm::new();
        let mut msg = content_job(&[0xFF], flag::SETTIMER);
        msg.ival2_us = 100_000;
        bcm.request(&msg, 0).unwrap();
        // The first change goes out at once.
        bcm.on_frame(&frame(0x123, &[1]), 10 * MS, 10 * MS);
        assert_eq!(reported(&mut bcm).len(), 1);
        // Two more within the 100 ms: held back, the later one wins.
        bcm.on_frame(&frame(0x123, &[2]), 30 * MS, 30 * MS);
        bcm.on_frame(&frame(0x123, &[3]), 60 * MS, 60 * MS);
        assert_eq!(reported(&mut bcm), []);
        assert_eq!(bcm.next_deadline(), Some(110 * MS));
        bcm.poll(110 * MS);
        assert_eq!(
            reported(&mut bcm),
            [(60 * MS, op::RX_CHANGED, vec![frame(0x123, &[3])])]
        );
        // Nothing new by the next slot: the spacing ends, and a change
        // after that goes out at once again.
        assert_eq!(bcm.next_deadline(), Some(210 * MS));
        bcm.poll(210 * MS);
        assert_eq!(reported(&mut bcm), []);
        assert_eq!(bcm.next_deadline(), None);
        bcm.on_frame(&frame(0x123, &[4]), 300 * MS, 300 * MS);
        assert_eq!(reported(&mut bcm).len(), 1);
    }

    #[test]
    fn multiplexed_frames_are_compared_per_multiplexer_value() {
        let mut bcm = Bcm::new();
        // Byte 0 is the multiplexer; values 1 and 2 are watched, each in
        // its second byte.
        let msg = BcmMessage {
            opcode: op::RX_SETUP,
            can_id: 0x123,
            frames: vec![
                frame(0x123, &[0xFF, 0, 0, 0, 0, 0, 0, 0]),
                frame(0x123, &[0x01, 0xFF, 0, 0, 0, 0, 0, 0]),
                frame(0x123, &[0x02, 0xFF, 0, 0, 0, 0, 0, 0]),
            ],
            ..BcmMessage::default()
        };
        bcm.request(&msg, 0).unwrap();
        let mut counts = Vec::new();
        for data in [
            [1, 10], // first with multiplexer 1
            [2, 20], // first with multiplexer 2
            [1, 10], // multiplexer 1 unchanged
            [2, 21], // multiplexer 2 changed
            [3, 99], // a multiplexer value nobody watches
        ] {
            bcm.on_frame(&frame(0x123, &data), 0, 0);
            counts.push(reported(&mut bcm).len());
        }
        assert_eq!(counts, [1, 1, 0, 1, 0]);
    }

    #[test]
    fn after_a_timeout_the_same_frame_can_be_reported_again() {
        let setup = |flags| {
            let mut bcm = Bcm::new();
            let mut msg = content_job(&[0xFF], flags | flag::SETTIMER);
            msg.ival1_us = 50_000;
            bcm.request(&msg, 0).unwrap();
            let f = frame(0x123, &[1]);
            bcm.on_frame(&f, 0, 0);
            bcm.poll(50 * MS);
            bcm.on_frame(&f, 60 * MS, 60 * MS);
            reported(&mut bcm).iter().map(|r| r.1).collect::<Vec<u32>>()
        };
        assert_eq!(setup(0), [op::RX_CHANGED, op::RX_TIMEOUT]);
        assert_eq!(
            setup(flag::RX_ANNOUNCE_RESUME),
            [op::RX_CHANGED, op::RX_TIMEOUT, op::RX_CHANGED]
        );
    }

    #[test]
    fn a_remote_frame_is_answered_with_the_frame_given() {
        let mut bcm = Bcm::new();
        let remote_id = 0x123 | CAN_RTR_FLAG;
        let msg = BcmMessage {
            opcode: op::RX_SETUP,
            flags: flag::RX_RTR_FRAME | flag::TX_CP_CAN_ID,
            can_id: remote_id,
            frames: vec![frame(0x7FF, &[0xCA, 0xFE])],
            ..BcmMessage::default()
        };
        bcm.request(&msg, 0).unwrap();
        // A data frame with the identifier is not a request.
        bcm.on_frame(&frame(0x123, &[1]), 0, 0);
        assert_eq!(sent(&mut bcm), []);
        bcm.on_frame(&Frame::new_remote(Id::Standard(0x123), 2).unwrap(), 1, 1);
        assert_eq!(sent(&mut bcm), [frame(0x123, &[0xCA, 0xFE])]);
        assert_eq!(reported(&mut bcm), []);

        // It needs the remote flag in the identifier and exactly one frame.
        let mut wrong = msg.clone();
        wrong.can_id = 0x123;
        assert!(bcm.request(&wrong, 0).is_err());
    }

    #[test]
    fn can_fd_jobs_see_only_can_fd_frames() {
        let mut bcm = Bcm::new();
        let mut msg = BcmMessage::rx_watch(0x123, 0);
        msg.flags |= flag::CAN_FD_FRAME;
        bcm.request(&msg, 0).unwrap();
        assert_eq!(bcm.watched(), [(0x123, true)]);
        bcm.on_frame(&frame(0x123, &[1]), 0, 0);
        assert_eq!(reported(&mut bcm), []);
        let fd = Frame::new_fd(Id::Standard(0x123), &[1; 12], true).unwrap();
        bcm.on_frame(&fd, 1, 1);
        assert_eq!(reported(&mut bcm), [(1, op::RX_CHANGED, vec![fd])]);
    }

    #[test]
    fn a_receive_job_is_read_back_and_deleted() {
        let mut bcm = Bcm::new();
        bcm.request(&content_job(&[0xF0], 0), 0).unwrap();
        let read = BcmMessage {
            opcode: op::RX_READ,
            can_id: 0x123,
            ..BcmMessage::default()
        };
        bcm.request(&read, 5).unwrap();
        let (_, status) = bcm.poll_report().unwrap();
        assert_eq!(status.opcode, op::RX_STATUS);
        assert_eq!(status.frames, [frame(0x123, &[0xF0])]);
        let delete = BcmMessage {
            opcode: op::RX_DELETE,
            can_id: 0x123,
            ..BcmMessage::default()
        };
        bcm.request(&delete, 6).unwrap();
        assert!(bcm.request(&delete, 7).is_err());
        assert!(bcm.request(&read, 8).is_err());
        assert_eq!(bcm.watched(), []);
    }
}
