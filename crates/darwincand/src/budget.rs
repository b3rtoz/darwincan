// SPDX-License-Identifier: GPL-2.0-only

//! How much message data the daemon holds, for each connection and in
//! all: what a client has given it to send (ISO-TP and J1939 messages),
//! and what is queued for a client to read. CAN frames are not counted:
//! they are small and their queues are limited by number.
//!
//! A client that wants to send more than there is room for waits, as a
//! program does on a socket whose buffer is full: the daemon stops
//! reading from it. A message for a client that is not reading what it
//! has is dropped, as frames are. Nothing here is on the frame path.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// What one connection may have waiting to be sent, and what may be
/// queued for it to read: together what one connection can make the
/// daemon hold. Each fits the longest message there is.
pub(crate) const CONNECTION_SENDING: usize = 128 << 20;
pub(crate) const CONNECTION_QUEUED: usize = 128 << 20;
/// What all connections together may make the daemon hold, unless it is
/// told otherwise.
pub const DEFAULT_LIMIT: usize = 1 << 30;
/// A request with a payload of at least this is counted before it is
/// read. Smaller ones are limited by how many may be queued.
pub(crate) const COUNTED_FROM: usize = 64 << 10;

#[derive(Default)]
struct Totals {
    sending: usize,
    queued: usize,
}

pub(crate) struct Budget {
    limit: AtomicUsize,
    totals: Mutex<Totals>,
    room: Condvar,
}

impl Budget {
    pub(crate) fn new() -> Arc<Budget> {
        Arc::new(Budget {
            limit: AtomicUsize::new(DEFAULT_LIMIT),
            totals: Mutex::new(Totals::default()),
            room: Condvar::new(),
        })
    }

    pub(crate) fn set_limit(&self, bytes: usize) {
        self.limit.store(bytes, Ordering::Relaxed);
        self.room.notify_all();
    }

    /// How much is held to be sent, and how much is queued to be read.
    pub(crate) fn held(&self) -> (usize, usize) {
        let totals = self.totals.lock().unwrap();
        (totals.sending, totals.queued)
    }

    fn released(&self) {
        self.room.notify_all();
    }
}

/// One connection's share. Its counts change only under the budget's
/// lock; when the connection has gone, what is left of them is given
/// back.
pub(crate) struct Account {
    budget: Arc<Budget>,
    sending: AtomicUsize,
    queued: AtomicUsize,
}

impl Account {
    pub(crate) fn new(budget: &Arc<Budget>) -> Arc<Account> {
        Arc::new(Account {
            budget: budget.clone(),
            sending: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
        })
    }

    /// Take room for `bytes` to be sent, waiting until there is some. A
    /// connection that has nothing waiting gets its message whatever its
    /// size, and so does the daemon as a whole: otherwise a message
    /// longer than a limit could never be sent.
    pub(crate) fn reserve(&self, bytes: usize) {
        let mut totals = self.budget.totals.lock().unwrap();
        loop {
            let mine = self.sending.load(Ordering::Relaxed);
            let limit = self.budget.limit.load(Ordering::Relaxed);
            let fits_here = mine == 0 || mine + bytes <= CONNECTION_SENDING;
            let fits_at_all =
                totals.sending == 0 || totals.sending + totals.queued + bytes <= limit;
            if fits_here && fits_at_all {
                break;
            }
            totals = self.budget.room.wait(totals).unwrap();
        }
        self.sending.fetch_add(bytes, Ordering::Relaxed);
        totals.sending += bytes;
    }

    /// Count `bytes` more as waiting to be sent, without waiting: for
    /// what the daemon has taken already.
    pub(crate) fn add_sending(&self, bytes: usize) {
        let mut totals = self.budget.totals.lock().unwrap();
        self.sending.fetch_add(bytes, Ordering::Relaxed);
        totals.sending += bytes;
    }

    /// `bytes` are no longer waiting to be sent.
    pub(crate) fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let mut totals = self.budget.totals.lock().unwrap();
        let bytes = bytes.min(self.sending.load(Ordering::Relaxed));
        self.sending.fetch_sub(bytes, Ordering::Relaxed);
        totals.sending -= bytes;
        drop(totals);
        self.budget.released();
    }

    /// Count a message of `bytes` as queued for the client to read.
    /// False if there is no room: the message is then dropped.
    ///
    /// A client with nothing waiting always gets its message: one that
    /// reads what it is sent must not lose messages because others do
    /// not. Beyond that, a client's share is limited, and so is what all
    /// clients have waiting, to half of what the daemon may hold: the
    /// other half is kept for sending, so that clients that do not read
    /// cannot stop the others from sending.
    pub(crate) fn queue(&self, bytes: usize) -> bool {
        let mut totals = self.budget.totals.lock().unwrap();
        let mine = self.queued.load(Ordering::Relaxed);
        let limit = self.budget.limit.load(Ordering::Relaxed) / 2;
        if mine > 0 && (mine + bytes > CONNECTION_QUEUED || totals.queued + bytes > limit) {
            return false;
        }
        self.queued.fetch_add(bytes, Ordering::Relaxed);
        totals.queued += bytes;
        true
    }

    /// A queued message of `bytes` has been written to the client, or
    /// could not be queued after all.
    pub(crate) fn unqueue(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let mut totals = self.budget.totals.lock().unwrap();
        let bytes = bytes.min(self.queued.load(Ordering::Relaxed));
        self.queued.fetch_sub(bytes, Ordering::Relaxed);
        totals.queued -= bytes;
        drop(totals);
        self.budget.released();
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        let (sending, queued) = (
            self.sending.load(Ordering::Relaxed),
            self.queued.load(Ordering::Relaxed),
        );
        if sending + queued == 0 {
            return;
        }
        let mut totals = self.budget.totals.lock().unwrap();
        totals.sending -= sending;
        totals.queued -= queued;
        drop(totals);
        self.budget.released();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn a_connection_waits_for_room_of_its_own() {
        let budget = Budget::new();
        let account = Account::new(&budget);
        // The first message is taken whatever its size.
        account.reserve(CONNECTION_SENDING + 1);
        let (done, waited) = mpsc::channel();
        let second = account.clone();
        thread::spawn(move || {
            second.reserve(1000);
            done.send(()).unwrap();
        });
        assert!(waited.recv_timeout(Duration::from_millis(100)).is_err());
        account.release(CONNECTION_SENDING + 1);
        waited.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(budget.held(), (1000, 0));
        // Several that fit together do not wait.
        account.reserve(CONNECTION_SENDING - 1000);
        assert_eq!(budget.held(), (CONNECTION_SENDING, 0));
    }

    #[test]
    fn all_connections_together_wait_for_room_in_the_daemon() {
        let budget = Budget::new();
        budget.set_limit(1 << 20);
        let (a, b) = (Account::new(&budget), Account::new(&budget));
        a.reserve(900_000);
        let (done, waited) = mpsc::channel();
        let other = b.clone();
        thread::spawn(move || {
            other.reserve(200_000);
            done.send(()).unwrap();
        });
        assert!(waited.recv_timeout(Duration::from_millis(100)).is_err());
        // A connection that goes gives back what it had.
        drop(a);
        waited.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(budget.held(), (200_000, 0));
        b.release(200_000);
        assert_eq!(budget.held(), (0, 0));
    }

    #[test]
    fn what_a_client_does_not_read_is_dropped_beyond_its_share() {
        let budget = Budget::new();
        let account = Account::new(&budget);
        assert!(account.queue(CONNECTION_QUEUED - 10));
        assert!(account.queue(10));
        assert!(!account.queue(1));
        account.unqueue(10);
        assert!(account.queue(10));
        // Half of what the daemon may hold is kept for sending: with that
        // much waiting already, another client gets the message it has
        // nothing in front of, and no second one.
        budget.set_limit(CONNECTION_QUEUED);
        let other = Account::new(&budget);
        assert!(other.queue(1));
        assert!(!other.queue(1));
        // Queued data does not stop a first send.
        other.reserve(5);
        assert_eq!(budget.held(), (5, CONNECTION_QUEUED + 1));
    }
}
