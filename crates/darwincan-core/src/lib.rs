// SPDX-License-Identifier: MIT OR Apache-2.0

//! Types shared between the `darwincand` daemon and its clients.
//!
//! Frame identifiers, flags and filter semantics deliberately follow Linux
//! SocketCAN (`<linux/can.h>`) so that code and habits carry over.

pub mod bcm;
mod config;
mod frame;
mod isotp;
pub mod j1939;
pub mod wire;

use std::path::PathBuf;

pub use bcm::BcmMessage;
pub use config::BusConfig;
pub use frame::err;
pub use frame::{
    fd_len_round_up, is_valid_fd_len, Filter, Frame, FrameError, Id, CANFD_BRS, CANFD_ESI,
    CANFD_FDF, CANFD_MAX_DLEN, CAN_EFF_FLAG, CAN_EFF_MASK, CAN_ERR_FLAG, CAN_INV_FILTER,
    CAN_MAX_DLEN, CAN_RTR_FLAG, CAN_SFF_MASK,
};
pub use isotp::{IsoTpFailure, IsoTpParams};
pub use j1939::{J1939Addr, J1939Filter, J1939Message, J1939Notice, J1939NoticeKind};

/// Environment variable that overrides the daemon socket location.
pub const SOCKET_ENV: &str = "DARWINCAN_SOCKET";

/// Where the daemon listens and clients connect by default.
///
/// `$DARWINCAN_SOCKET` if set, otherwise a per-user path under `$HOME` (not a
/// shared temp directory, so another local user cannot pre-create it).
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os(SOCKET_ENV) {
        return PathBuf::from(p);
    }
    match std::env::var_os("HOME") {
        Some(home) => {
            PathBuf::from(home).join("Library/Application Support/darwincan/darwincand.sock")
        }
        None => PathBuf::from("/tmp/darwincand.sock"),
    }
}

/// True if `name` is acceptable as an interface name (`can0`, `vcan0`, ...).
/// `any` is not: it stands for every interface.
pub fn is_valid_ifname(name: &str) -> bool {
    name != wire::ANY_INTERFACE
        && (1..=15).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
