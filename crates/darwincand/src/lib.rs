// SPDX-License-Identifier: GPL-2.0-only

//! The darwinCAN daemon.
//!
//! macOS lets only one process claim a USB interface, so a CAN adapter can
//! normally serve a single application. The daemon is that one process: it
//! owns the adapters, exposes each channel as a named interface (`can0`,
//! `vcan0`, ...) and fans frames out to any number of clients over a Unix
//! socket, giving them SocketCAN-like semantics (filters, loopback,
//! timestamps).
//!
//! Timing-sensitive protocol work runs here, next to the bus, rather than in
//! clients: ISO-TP flow control and STmin pacing never wait on a round trip
//! through a client process.

pub mod backends;
mod bcm;
pub mod bittiming;
mod block_on;
mod budget;
mod bus;
mod clock;
mod j1939;
pub mod large_alloc;
pub mod launchd;
mod names;
mod priority;
mod session;
mod signal;
pub mod timing;

pub use budget::DEFAULT_LIMIT as DEFAULT_MEMORY_LIMIT;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::time::Duration;

use darwincan_core::{is_valid_ifname, BusConfig, Frame};

use bus::Bus;
pub use clock::now_ns;

/// A hardware channel driver. The daemon calls [`transmit`](Self::transmit)
/// for outgoing frames; the driver reports incoming ones through the
/// [`InterfaceHandle`] it got when the interface was registered.
pub trait Backend: Send + Sync {
    /// Short driver name shown in interface listings, e.g. `"gs_usb"`.
    fn kind(&self) -> &str;
    /// Device model and serial number for interface listings.
    fn description(&self) -> String {
        String::new()
    }
    /// Apply the configuration and go on the bus. Never called while
    /// already started.
    fn start(&self, config: &BusConfig) -> io::Result<()>;
    /// Go off the bus.
    fn stop(&self) -> io::Result<()>;
    /// Go on the bus again after the adapter lost its state without being
    /// stopped (the computer slept). Override when the adapter keeps state
    /// that an ordinary stop and start would not set up again.
    fn restore(&self, config: &BusConfig) -> io::Result<()> {
        // The adapter may not know it was started; a failure to stop says
        // nothing about whether it can be started again.
        let _ = self.stop();
        self.start(config)
    }
    /// Whether the adapter reports each frame once it has been sent and
    /// acknowledged on the bus. If it does, the backend passes every such
    /// report to [`InterfaceHandle::transmitted`], and clients see a
    /// frame they sent when it was on the bus, stamped by the adapter. If
    /// not, they see it when it was handed over, stamped by the daemon.
    fn confirms_transmission(&self) -> bool {
        false
    }
    /// Hand a frame to the adapter for transmission, without waiting for
    /// it to be sent. Only called between `start` and `stop`. `token`
    /// names the frame in the report to [`InterfaceHandle::transmitted`].
    fn transmit(&self, frame: &Frame, token: u32) -> io::Result<()>;
}

/// Lets a backend's receive path deliver frames to an interface's clients.
#[derive(Clone)]
pub struct InterfaceHandle {
    bus: Arc<Bus>,
    shared: Arc<Shared>,
}

impl InterfaceHandle {
    /// Deliver a frame received from the wire. Pass the adapter's hardware
    /// timestamp converted to the [`now_ns`] time base if it has one;
    /// `None` stamps the frame on arrival.
    pub fn receive(&self, frame: &Frame, timestamp_ns: Option<u64>) {
        self.bus.receive(frame, timestamp_ns.unwrap_or_else(now_ns));
    }

    /// Report that the adapter sent `frame` on the bus and it was
    /// acknowledged, for a backend that
    /// [confirms transmission](Backend::confirms_transmission). `token` is
    /// the one given to [`Backend::transmit`]; `frame` and the timestamp
    /// are the adapter's own account. A token the bus no longer knows (the
    /// interface was restarted since) is still a frame that was on the
    /// bus, and clients see it as one.
    pub fn transmitted(&self, token: u32, frame: &Frame, timestamp_ns: Option<u64>) {
        self.bus
            .transmitted(token, frame, timestamp_ns.unwrap_or_else(now_ns));
    }

    pub fn name(&self) -> String {
        self.bus.name()
    }

    /// Remove the interface (its adapter was unplugged). Clients attached
    /// to it are told and their transmits fail from now on.
    pub fn remove(&self) {
        self.shared.buses.write().unwrap().remove(&self.bus.name());
        self.bus.mark_removed();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddInterfaceError {
    InvalidName,
    AlreadyExists,
    /// The name is kept for an adapter that is not plugged in.
    Reserved,
}

impl std::fmt::Display for AddInterfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AddInterfaceError::InvalidName => {
                "invalid interface name (1-15 letters, digits, '_' or '-')"
            }
            AddInterfaceError::AlreadyExists => "interface already exists",
            AddInterfaceError::Reserved => "the name is kept for an adapter that is not plugged in",
        })
    }
}

impl std::error::Error for AddInterfaceError {}

/// Attaches a socket that is open on every interface to one more.
type Attach = Box<dyn Fn(&Arc<Bus>) + Send + Sync>;

pub(crate) struct Shared {
    buses: RwLock<BTreeMap<String, Arc<Bus>>>,
    next_id: AtomicU64,
    next_index: AtomicU32,
    /// The sockets open on every interface, by their sink's id: each is
    /// attached to an interface as it appears. Locked after `buses`.
    everywhere: RwLock<Vec<(u64, Attach)>>,
    /// The names of hardware interfaces. Locked after `buses`.
    names: Mutex<names::Names>,
    /// The J1939 stacks, one for each interface with a J1939 socket.
    j1939: j1939::Nodes,
    /// How much message data is held for clients.
    budget: Arc<budget::Budget>,
}

/// Why an interface could not be renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenameError {
    NoSuchInterface,
    InvalidName,
    AlreadyExists,
    /// The name is kept for an adapter that is not plugged in.
    Reserved,
    /// A hardware interface has to be off the bus.
    Up,
}

impl Shared {
    pub(crate) fn bus(&self, name: &str) -> Option<Arc<Bus>> {
        self.buses.read().unwrap().get(name).cloned()
    }

    pub(crate) fn bus_by_index(&self, index: u32) -> Option<Arc<Bus>> {
        self.buses
            .read()
            .unwrap()
            .values()
            .find(|b| b.index == index)
            .cloned()
    }

    /// Make a bus, enter it under `name` and attach the sockets that are
    /// open on every interface. The caller holds the list of buses.
    fn insert(
        &self,
        buses: &mut BTreeMap<String, Arc<Bus>>,
        name: &str,
        identity: Option<String>,
        backend: Option<Box<dyn Backend>>,
    ) -> Arc<Bus> {
        let index = self.next_index.fetch_add(1, Ordering::Relaxed);
        let bus = Arc::new(Bus::with_identity(name, index, identity, backend));
        buses.insert(name.to_string(), bus.clone());
        for (_, attach) in self.everywhere.read().unwrap().iter() {
            attach(&bus);
        }
        bus
    }

    /// Give an interface another name, and keep it for a hardware
    /// interface's adapter. Sockets open on it stay open.
    pub(crate) fn rename(&self, name: &str, new_name: &str) -> Result<(), RenameError> {
        let mut buses = self.buses.write().unwrap();
        let bus = buses
            .get(name)
            .cloned()
            .ok_or(RenameError::NoSuchInterface)?;
        if !is_valid_ifname(new_name) {
            return Err(RenameError::InvalidName);
        }
        if name == new_name {
            return Ok(());
        }
        if buses.contains_key(new_name) {
            return Err(RenameError::AlreadyExists);
        }
        if !bus.is_virtual() && bus.is_up() {
            return Err(RenameError::Up);
        }
        let mut names = self.names.lock().unwrap();
        if names.belongs_to_another(new_name, bus.identity.as_deref()) {
            return Err(RenameError::Reserved);
        }
        if let Some(identity) = &bus.identity {
            if let Err(e) = names.rename(identity, new_name) {
                backends::log(format!(
                    "cannot keep the name {new_name} for next time ({e})"
                ));
            }
        }
        buses.remove(name);
        bus.set_name(new_name);
        buses.insert(new_name.to_string(), bus);
        Ok(())
    }

    /// Open a socket on every interface there is and will be: `attach`
    /// is run for each. `acknowledge` runs once they are all attached and
    /// before another can appear.
    pub(crate) fn attach_everywhere(&self, id: u64, attach: Attach, acknowledge: impl FnOnce()) {
        let buses = self.buses.read().unwrap();
        for bus in buses.values() {
            attach(bus);
        }
        self.everywhere.write().unwrap().push((id, attach));
        acknowledge();
    }

    /// Close a socket that was open on every interface.
    pub(crate) fn detach_everywhere(&self, id: u64) {
        let buses = self.buses.read().unwrap();
        self.everywhere.write().unwrap().retain(|(i, _)| *i != id);
        for bus in buses.values() {
            bus.remove_sink(id);
        }
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn add_bus(
        &self,
        name: &str,
        backend: Option<Box<dyn Backend>>,
    ) -> Result<Arc<Bus>, AddInterfaceError> {
        if !is_valid_ifname(name) {
            return Err(AddInterfaceError::InvalidName);
        }
        let mut buses = self.buses.write().unwrap();
        if buses.contains_key(name) {
            return Err(AddInterfaceError::AlreadyExists);
        }
        // A name kept for an adapter that is away stays free for it.
        if backend.is_none() && self.names.lock().unwrap().belongs_to_another(name, None) {
            return Err(AddInterfaceError::Reserved);
        }
        Ok(self.insert(&mut buses, name, None, backend))
    }
}

#[derive(Clone)]
pub struct Daemon {
    shared: Arc<Shared>,
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

impl Daemon {
    pub fn new() -> Daemon {
        Daemon {
            shared: Arc::new(Shared {
                buses: RwLock::new(BTreeMap::new()),
                next_id: AtomicU64::new(1),
                next_index: AtomicU32::new(1),
                everywhere: RwLock::new(Vec::new()),
                names: Mutex::new(names::Names::default()),
                j1939: j1939::Nodes::default(),
                budget: budget::Budget::new(),
            }),
        }
    }

    /// Set how many bytes of message data (ISO-TP and J1939 messages
    /// waiting to be sent or to be read) all clients together may make
    /// the daemon hold: [`DEFAULT_MEMORY_LIMIT`] unless set.
    pub fn set_memory_limit(&self, bytes: usize) {
        self.shared.budget.set_limit(bytes);
    }

    /// How many bytes of message data the daemon holds now: waiting to be
    /// sent, and queued for clients to read.
    pub fn memory_held(&self) -> (usize, usize) {
        self.shared.budget.held()
    }

    /// Create a virtual bus: frames sent by one client reach the others,
    /// with no hardware involved.
    pub fn add_vcan(&self, name: &str) -> Result<(), AddInterfaceError> {
        self.shared.add_bus(name, None).map(|_| ())
    }

    /// Register a hardware channel under `name`.
    pub fn add_interface(
        &self,
        name: &str,
        backend: Box<dyn Backend>,
    ) -> Result<InterfaceHandle, AddInterfaceError> {
        let bus = self.shared.add_bus(name, Some(backend))?;
        Ok(InterfaceHandle {
            bus,
            shared: self.shared.clone(),
        })
    }

    /// Keep the names of hardware interfaces in the file at `path`, so
    /// that each adapter channel has the same name in every run. Call
    /// before any adapter is attached. Without it names are kept for
    /// this run only.
    pub fn keep_names_in(&self, path: &Path) {
        *self.shared.names.lock().unwrap() = names::Names::load(path);
    }

    /// Register a hardware channel under the name kept for it, or the
    /// first `can<N>` that is free and kept for no other. `identity` says
    /// which adapter and which of its channels this is, in words that are
    /// the same every time it is attached.
    pub fn add_can_interface(&self, identity: &str, backend: Box<dyn Backend>) -> InterfaceHandle {
        let mut buses = self.shared.buses.write().unwrap();
        let name = self
            .shared
            .names
            .lock()
            .unwrap()
            .name_for(identity, |name| buses.contains_key(name));
        let bus = self
            .shared
            .insert(&mut buses, &name, Some(identity.to_string()), Some(backend));
        InterfaceHandle {
            bus,
            shared: self.shared.clone(),
        }
    }

    /// Put every hardware interface that is up back on the bus with its
    /// settings. Returns the names of those that could not be restored,
    /// which are left up for another attempt.
    pub fn restore_interfaces(&self) -> Vec<String> {
        let buses: Vec<Arc<Bus>> = self
            .shared
            .buses
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let mut failed = Vec::new();
        for bus in buses {
            match bus.restore() {
                None => {}
                Some(Ok(())) => backends::log(format!("{} restored", bus.name())),
                Some(Err(e)) => {
                    backends::log(format!("{} could not be restored: {e}", bus.name()));
                    failed.push(bus.name());
                }
            }
        }
        failed
    }

    /// Watch for the computer waking from sleep and restore the hardware
    /// interfaces when it does: adapters lose their bus settings while it
    /// sleeps.
    pub fn restore_after_sleep(&self) -> io::Result<()> {
        const POLL: std::time::Duration = std::time::Duration::from_secs(1);
        /// USB devices take a moment to come back after a wake.
        const ATTEMPTS: usize = 5;
        let daemon = self.clone();
        thread::Builder::new()
            .name("darwincand-wake".into())
            .spawn(move || {
                let mut asleep = clock::asleep_ns();
                loop {
                    thread::sleep(POLL);
                    let now = clock::asleep_ns();
                    // Any sleep counts: an idle sleep that a network
                    // packet ends within a second still cuts the
                    // adapters' power. The margin is only for the two
                    // clocks not being read in the same instant.
                    if now.saturating_sub(asleep) < 10_000_000 {
                        continue;
                    }
                    asleep = now;
                    backends::log("woke from sleep, restoring interfaces");
                    for attempt in 1..=ATTEMPTS {
                        let failed = daemon.restore_interfaces();
                        if failed.is_empty() {
                            break;
                        }
                        if attempt == ATTEMPTS {
                            // Do not leave an interface claiming to be up.
                            for name in failed {
                                if let Some(bus) = daemon.shared.bus(&name) {
                                    let _ = bus.stop();
                                    backends::log(format!("{name} is now down"));
                                }
                            }
                        } else {
                            thread::sleep(POLL);
                        }
                    }
                }
            })?;
        Ok(())
    }

    /// Take every hardware interface that is up off the bus, so that no
    /// adapter goes on acknowledging frames with nothing behind it. The
    /// settings are kept, as with an ordinary "down".
    pub fn stop_interfaces(&self) {
        let buses: Vec<Arc<Bus>> = self
            .shared
            .buses
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for bus in buses {
            if bus.is_virtual() || !bus.is_up() {
                continue;
            }
            match bus.stop() {
                Ok(()) => backends::log(format!("{} taken off the bus", bus.name())),
                Err(e) => backends::log(format!("{} could not be stopped: {e}", bus.name())),
            }
        }
    }

    /// When the daemon is told to stop by a signal (interrupt, terminate
    /// or hang-up), take the hardware interfaces off the bus before the
    /// process ends. Call before starting any other thread: threads
    /// started earlier would still be killed by the signal directly.
    pub fn stop_on_signal(&self) -> io::Result<()> {
        /// A stuck adapter must not keep the daemon from stopping.
        const LIMIT: Duration = Duration::from_secs(2);
        signal::block()?;
        let daemon = self.clone();
        thread::Builder::new()
            .name("darwincand-signal".into())
            .spawn(move || {
                let signal = signal::wait();
                let (done, finished) = mpsc::channel();
                // If the thread cannot be started the channel closes and
                // the wait below returns at once.
                let _ = thread::Builder::new()
                    .name("darwincand-stop".into())
                    .spawn(move || {
                        daemon.stop_interfaces();
                        let _ = done.send(());
                    });
                let _ = finished.recv_timeout(LIMIT);
                signal::die(signal)
            })?;
        Ok(())
    }

    /// Accept and serve clients until the listener fails. Each client gets
    /// its own threads.
    pub fn serve(&self, listener: UnixListener) -> io::Result<()> {
        // One client must not be able to end the daemon for the others: a
        // connection that cannot be accepted or served (no descriptors
        // left, no thread to be had, a client gone again already) is
        // given up and the next one waited for. Only a listener that
        // fails every time is fatal.
        const GIVE_UP_AFTER: u32 = 1000;
        let mut failures = 0;
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    failures = 0;
                    let shared = self.shared.clone();
                    let spawned = thread::Builder::new()
                        .name("darwincand-client".into())
                        .spawn(move || session::run(shared, stream));
                    if let Err(e) = spawned {
                        backends::log(format!("cannot serve a client: {e}"));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    failures += 1;
                    if failures >= GIVE_UP_AFTER {
                        return Err(e);
                    }
                    if failures == 1 {
                        backends::log(format!("cannot accept a client: {e}"));
                    }
                    // Out of descriptors, most likely: wait for a client
                    // to go rather than spin.
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
        Ok(())
    }
}

extern "C" {
    fn getrlimit(resource: i32, limit: *mut ResourceLimit) -> i32;
    fn setrlimit(resource: i32, limit: *const ResourceLimit) -> i32;
}

/// `struct rlimit` of `<sys/resource.h>`.
#[repr(C)]
struct ResourceLimit {
    current: u64,
    maximum: u64,
}

const RLIMIT_NOFILE: i32 = 8;
/// The most open files macOS lets a process ask for (`OPEN_MAX`).
const OPEN_MAX: u64 = 10240;

/// Let the daemon have as many open files as the system allows. Every
/// client connection takes three descriptors, and a process started from
/// a shell or by launchd is given 256.
pub fn raise_descriptor_limit() {
    let mut limit = ResourceLimit {
        current: 0,
        maximum: 0,
    };
    // SAFETY: both calls are given a valid `struct rlimit`.
    unsafe {
        if getrlimit(RLIMIT_NOFILE, &mut limit) == 0 {
            let wanted = limit.maximum.min(OPEN_MAX);
            if wanted > limit.current {
                limit.current = wanted;
                setrlimit(RLIMIT_NOFILE, &limit);
            }
        }
    }
}

/// Create the listening socket at `path`, readable and writable by the
/// owner only. A socket file left behind by a daemon that is no longer
/// running is replaced; a live daemon makes this fail with `AddrInUse`.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    if let Ok(found) = fs::symlink_metadata(path) {
        // Only ever a socket is removed: a mistyped path must not cost
        // someone a file.
        if !found.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another darwincand is already listening on this socket",
            ));
        }
        fs::remove_file(path)?;
    }
    // Narrowed to the owner straight after it is made. The file mode mask
    // would close the moment in between, but it belongs to the whole
    // process, and other threads make files. With the usual mask nobody
    // else may write to the socket, and so connect, in that moment
    // either; and the default place is in a directory others cannot
    // enter.
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}
