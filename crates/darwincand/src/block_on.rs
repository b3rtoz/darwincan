// SPDX-License-Identifier: GPL-2.0-or-later

//! Running a future to completion on the current thread. The USB library
//! reports adapters being plugged in as a stream; the daemon uses plain
//! threads.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

struct ThreadWaker {
    thread: Thread,
    woken: AtomicBool,
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// Block until `future` completes.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let state = Arc::new(ThreadWaker {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(state.clone());
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        // Park until woken; parking can return spuriously, hence the flag.
        while !state.woken.swap(false, Ordering::Acquire) {
            thread::park();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Completes when a value is sent from another thread.
    struct Recv(mpsc::Receiver<u32>, Arc<std::sync::Mutex<Option<Waker>>>);

    impl Future for Recv {
        type Output = u32;
        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
            *self.1.lock().unwrap() = Some(cx.waker().clone());
            match self.0.try_recv() {
                Ok(v) => Poll::Ready(v),
                Err(_) => Poll::Pending,
            }
        }
    }

    #[test]
    fn completes_when_woken_from_another_thread() {
        let (tx, rx) = mpsc::channel();
        let slot = Arc::new(std::sync::Mutex::new(None::<Waker>));
        let slot2 = slot.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.send(7).unwrap();
            if let Some(w) = slot2.lock().unwrap().take() {
                w.wake();
            }
        });
        assert_eq!(block_on(Recv(rx, slot)), 7);
    }
}
