// SPDX-License-Identifier: MIT OR Apache-2.0

/// How a hardware interface joins the bus (the `ip link set canX up type can
/// ...` parameters on Linux).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusConfig {
    /// Nominal (arbitration phase) bitrate in bit/s.
    pub bitrate: u32,
    /// CAN FD data phase bitrate in bit/s; `None` for classic CAN only.
    pub data_bitrate: Option<u32>,
    /// Nominal sample point in tenths of a percent (875 = 87.5 %);
    /// `None` picks the CiA-recommended value for the bitrate.
    pub sample_point: Option<u16>,
    /// Data phase sample point, same units.
    pub data_sample_point: Option<u16>,
    /// Receive only: never transmit, acknowledge or send error frames.
    pub listen_only: bool,
}

impl BusConfig {
    /// Classic CAN at `bitrate` with default sample point.
    pub fn new(bitrate: u32) -> BusConfig {
        BusConfig {
            bitrate,
            data_bitrate: None,
            sample_point: None,
            data_sample_point: None,
            listen_only: false,
        }
    }

    /// CAN FD with separate arbitration and data bitrates.
    pub fn fd(bitrate: u32, data_bitrate: u32) -> BusConfig {
        BusConfig {
            data_bitrate: Some(data_bitrate),
            ..BusConfig::new(bitrate)
        }
    }
}
