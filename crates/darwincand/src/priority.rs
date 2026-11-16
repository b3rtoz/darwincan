// SPDX-License-Identifier: GPL-2.0-or-later

//! Scheduling priority for the threads that move frames.

/// Ask the scheduler to run the calling thread ahead of ordinary work.
///
/// On macOS this selects the user-interactive quality-of-service class,
/// which also keeps the thread on performance cores and exempts its
/// timers from coalescing. It is the fallback where the real-time class
/// is refused: a thread in it is still held up for 5 to 20 ms a few
/// times in ten minutes (once for 143 ms), which showed on the bus as
/// frames leaving late. Failure is harmless and ignored.
fn raise_current_thread() {
    #[cfg(target_os = "macos")]
    {
        // From <sys/qos.h>.
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }
        // SAFETY: plain libSystem call taking two integers and affecting
        // only the calling thread.
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }
}

/// Put the calling thread in the real-time scheduling class, for the
/// threads that move frames and those that wait on protocol timers.
///
/// An ordinary thread, even a user-interactive one, is woken late: from a
/// timed wait by about a millisecond on average and up to five in
/// measurements on Apple silicon, which stretches every ISO-TP STmin gap;
/// and from a wait for a frame by 5 to 20 ms now and then. A
/// time-constraint thread is woken within some tens of microseconds, and
/// in a 12-minute soak at 1 ms spacing no frame of 1.4 million left more
/// than half a millisecond late.
/// The kernel expects such a thread to run briefly and then block, which
/// is what these do; one that computes for long loses the class for a
/// while. Falls back to the user-interactive class if the request fails.
pub fn make_current_thread_realtime() {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain libSystem calls naming the calling thread.
        let thread = unsafe { mach::pthread_mach_thread_np(mach::pthread_self()) };
        if mach::make_realtime(thread) {
            return;
        }
    }
    raise_current_thread();
}

/// Put the thread of this process called `name` in the real-time class.
/// False if there is none or the request failed.
///
/// This is for the one thread the daemon does not start itself: the USB
/// library's, which hands every adapter's transfers to the daemon and is
/// of ordinary priority. Left like that, adapters' messages reached the
/// daemon up to 18 ms late every 40 seconds or so, and up to 82 ms late
/// through a USB dock; with it real-time, once by 3 ms in 12 minutes.
pub fn make_named_thread_realtime(name: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        mach::threads()
            .into_iter()
            .any(|(thread_name, thread)| thread_name == name && mach::make_realtime(thread))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = name;
        false
    }
}

#[cfg(target_os = "macos")]
mod mach {
    use std::ffi::c_void;

    // From <mach/thread_policy.h>.
    const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;

    #[repr(C)]
    struct TimeConstraint {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: u32,
    }
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    extern "C" {
        static mach_task_self_: u32;
        pub fn pthread_self() -> *mut c_void;
        pub fn pthread_mach_thread_np(thread: *mut c_void) -> u32;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
        fn thread_policy_set(
            thread: u32,
            flavor: u32,
            info: *const TimeConstraint,
            count: u32,
        ) -> i32;
        fn task_threads(task: u32, threads: *mut *mut u32, count: *mut u32) -> i32;
        fn pthread_from_mach_thread_np(thread: u32) -> *mut c_void;
        fn pthread_getname_np(thread: *mut c_void, name: *mut u8, len: usize) -> i32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
        fn vm_deallocate(task: u32, address: usize, size: usize) -> i32;
    }

    /// Put the thread named by a Mach port in the real-time class.
    pub fn make_realtime(thread: u32) -> bool {
        let mut timebase = Timebase { numer: 0, denom: 0 };
        // SAFETY: the pointer refers to a live local of the layout
        // mach_timebase_info fills in.
        if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase.numer == 0 {
            return false;
        }
        // The policy is given in mach time units, not nanoseconds.
        let ticks = |ns: u64| (ns * u64::from(timebase.denom) / u64::from(timebase.numer)) as u32;
        let policy = TimeConstraint {
            // No fixed period: the thread runs when a timer or a frame
            // wakes it.
            period: 0,
            // Each wake-up needs well under this much processor time
            // (the final spin before a deadline included)...
            computation: ticks(250_000),
            // ...and should have had it within this long.
            constraint: ticks(1_000_000),
            preemptible: 1,
        };
        // SAFETY: `policy` is four 32-bit integers, the count passed; a
        // port that names no thread makes the call fail.
        unsafe { thread_policy_set(thread, THREAD_TIME_CONSTRAINT_POLICY, &policy, 4) == 0 }
    }

    /// Every thread of this process that has a name: the name and the
    /// thread's Mach port, which is only good for use straight away.
    pub fn threads() -> Vec<(String, u32)> {
        let mut list: *mut u32 = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: task_threads hands back an array of `count` ports in
        // memory of ours, each with a reference; both are released below.
        // The name is written into a buffer of the length passed and is
        // terminated.
        unsafe {
            if task_threads(mach_task_self_, &mut list, &mut count) != 0 {
                return Vec::new();
            }
            let mut threads = Vec::new();
            for i in 0..count as usize {
                let thread = *list.add(i);
                let pthread = pthread_from_mach_thread_np(thread);
                let mut name = [0u8; 64];
                if !pthread.is_null()
                    && pthread_getname_np(pthread, name.as_mut_ptr(), name.len()) == 0
                {
                    let len = name.iter().position(|&b| b == 0).unwrap_or(0);
                    if len > 0 {
                        threads.push((String::from_utf8_lossy(&name[..len]).into_owned(), thread));
                    }
                }
                mach_port_deallocate(mach_task_self_, thread);
            }
            vm_deallocate(
                mach_task_self_,
                list as usize,
                count as usize * std::mem::size_of::<u32>(),
            );
            threads
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn a_thread_is_found_by_its_name() {
        let (started, wait) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("priority-test".into())
            .spawn(move || {
                started.send(()).unwrap();
                let _ = held.recv();
            })
            .unwrap();
        wait.recv().unwrap();
        assert!(make_named_thread_realtime("priority-test"));
        assert!(!make_named_thread_realtime("no-such-thread"));
        release.send(()).unwrap();
        thread.join().unwrap();
    }
}
