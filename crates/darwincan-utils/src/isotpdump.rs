// SPDX-License-Identifier: MIT OR Apache-2.0

//! What `isotpdump` prints: one line for each frame of an ISO-TP exchange,
//! taken apart after ISO 15765-2, and with `-u` the name of the UDS
//! service after ISO 14229-1. Written from the standards and from the
//! output of can-utils' `isotpdump`, not from its source.

use darwincan::Frame;

/// Which of the two identifiers a frame has, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Source,
    Destination,
}

/// How the lines are to be printed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// The first data byte is an address (extended addressing).
    pub extended: bool,
    /// Show the data as text too.
    pub ascii: bool,
    /// Name the UDS service.
    pub uds: bool,
    /// Red for the source's frames, blue for the destination's.
    pub colour: bool,
}

const RED: &str = "\x1b[31m";
const BLUE: &str = "\x1b[34m";
const RESET: &str = "\x1b[0m";

/// The names of the UDS services (ISO 14229-1), by service identifier.
fn service(sid: u8) -> Option<&'static str> {
    Some(match sid {
        0x10 => "DiagnosticSessionControl",
        0x11 => "ECUReset",
        0x14 => "ClearDiagnosticInformation",
        0x19 => "ReadDTCInformation",
        0x22 => "ReadDataByIdentifier",
        0x23 => "ReadMemoryByAddress",
        0x24 => "ReadScalingDataByIdentifier",
        0x27 => "SecurityAccess",
        0x28 => "CommunicationControl",
        0x2A => "ReadDataByPeriodicIdentifier",
        0x2C => "DynamicallyDefineDataIdentifier",
        0x2E => "WriteDataByIdentifier",
        0x2F => "InputOutputControlByIdentifier",
        0x31 => "RoutineControl",
        0x34 => "RequestDownload",
        0x35 => "RequestUpload",
        0x36 => "TransferData",
        0x37 => "RequestTransferExit",
        0x38 => "RequestFileTransfer",
        0x3D => "WriteMemoryByAddress",
        0x3E => "TesterPresent",
        0x83 => "AccessTimingParameter",
        0x84 => "SecuredDataTransmission",
        0x85 => "ControlDTCSetting",
        0x86 => "ResponseOnEvent",
        0x87 => "LinkControl",
        _ => return None,
    })
}

/// The names of the negative response codes (ISO 14229-1).
fn negative_response(code: u8) -> &'static str {
    match code {
        0x00 => "positiveResponse",
        0x10 => "generalReject",
        0x11 => "serviceNotSupported",
        0x12 => "sub-functionNotSupported",
        0x13 => "incorrectMessageLengthOrInvalidFormat",
        0x14 => "responseTooLong",
        0x21 => "busyRepeatRequest",
        0x22 => "conditionsNotCorrect",
        0x24 => "requestSequenceError",
        0x25 => "noResponseFromSubnetComponent",
        0x26 => "FailurePreventsExecutionOfRequestedAction",
        0x31 => "requestOutOfRange",
        0x33 => "securityAccessDenied",
        0x35 => "invalidKey",
        0x36 => "exceedNumberOfAttempts",
        0x37 => "requiredTimeDelayNotExpired",
        0x70 => "uploadDownloadNotAccepted",
        0x71 => "transferDataSuspended",
        0x72 => "generalProgrammingFailure",
        0x73 => "wrongBlockSequenceCounter",
        0x78 => "requestCorrectlyReceived-ResponsePending",
        0x7E => "sub-functionNotSupportedInActiveSession",
        0x7F => "serviceNotSupportedInActiveSession",
        0x81 => "rpmTooHigh",
        0x82 => "rpmTooLow",
        0x83 => "engineIsRunning",
        0x84 => "engineIsNotRunning",
        0x85 => "engineRunTimeTooLow",
        0x86 => "temperatureTooHigh",
        0x87 => "temperatureTooLow",
        0x88 => "vehicleSpeedTooHigh",
        0x89 => "vehicleSpeedTooLow",
        0x8A => "throttle/PedalTooHigh",
        0x8B => "throttle/PedalTooLow",
        0x8C => "transmissionRangeNotInNeutral",
        0x8D => "transmissionRangeNotInGear",
        0x38..=0x4F => "reservedByExtendedDataLinkSecurityDocument",
        0x8F => "brakeSwitch(es)NotClosed (Brake Pedal not pressed or not applied)",
        0x94..=0xEF => "reservedForSpecificConditionsNotCorrect",
        0xF0..=0xFD => "vehicleManufacturerSpecificConditionsNotCorrect",
        0x90 => "shifterLeverNotInPark",
        0x91 => "torqueConverterClutchLocked",
        0x92 => "voltageTooHigh",
        0x93 => "voltageTooLow",
        _ => "ISOSAEReserved",
    }
}

/// What `-u` says of a message that begins with `data`: whether it is a
/// request, a positive response or a negative one, and which.
pub fn uds_text(data: &[u8]) -> Option<String> {
    let sid = *data.first()?;
    Some(if sid == 0x7F {
        let code = data.get(2).copied().unwrap_or(0);
        format!("[NRC] {}", negative_response(code))
    } else if let Some(name) = service(sid) {
        format!("[SRQ] {name}")
    } else if let Some(name) = sid.checked_sub(0x40).and_then(service) {
        format!("[PSR] {name}")
    } else {
        // The ranges the standard keeps for requests, and those of
        // the responses to them as the original names them.
        match sid {
            0x10..=0x3E | 0x83..=0x88 | 0xBA..=0xBE => "[SRQ] Unknown".to_string(),
            0x50..=0x7E | 0xC3..=0xC8 => "[PSR] Unknown".to_string(),
            _ => "[???] Unknown".to_string(),
        }
    })
}

/// The separation time a flow control frame asks for, as it is written.
fn st_min(value: u8) -> String {
    match value {
        0x00..=0x7F => format!("{value} ms"),
        0xF1..=0xF9 => format!("{} us", u32::from(value - 0xF0) * 100),
        _ => "reserved".to_string(),
    }
}

/// The data of a first, single or consecutive frame, with what follows it.
fn data_text(data: &[u8], first: bool, options: &Options) -> String {
    let mut s = String::from("data:");
    if !data.is_empty() {
        s.push(' ');
    }
    for b in data {
        s.push_str(&format!("{b:02X} "));
    }
    let after = if options.uds && first {
        uds_text(data).map(|t| format!(" - {t}"))
    } else if options.ascii {
        let text: String = data
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        Some(format!(" -  '{text}'"))
    } else {
        None
    };
    if let Some(after) = after {
        // What follows the data starts in one column for as many bytes
        // as a classic frame carries after its header: seven, or six
        // behind an address byte.
        let most: usize = if options.extended { 6 } else { 7 };
        let pad = 3 * most.saturating_sub(data.len());
        s.push_str(&" ".repeat(pad));
        s.push_str(&after);
    }
    s
}

/// The line for one frame: `stamp` (empty, or the time in brackets), the
/// interface, the identifier, the length, and the frame taken apart.
pub fn line(stamp: &str, ifname: &str, frame: &Frame, side: Side, options: &Options) -> String {
    let all = frame.data();
    let (address, d) = match (options.extended, all.split_first()) {
        (true, Some((address, rest))) => (format!("{{{address:02X}}}"), rest),
        _ => (String::new(), all),
    };
    let id = if frame.is_extended() {
        format!("{:08X}", frame.id().raw())
    } else {
        format!("{:03X}", frame.id().raw())
    };
    let length = format!("[{}]", all.len());
    let mut s = format!("{stamp} {ifname}  {id}{address} {length:>4}  ");
    let pci = d.first().copied();
    match pci.map(|p| p >> 4) {
        Some(0) => {
            // A single frame: its length in the low four bits or, when
            // those are zero, in the byte after.
            let (len, data) = match d[0] & 0x0F {
                0 => (usize::from(d.get(1).copied().unwrap_or(0)), d.get(2..)),
                n => (usize::from(n), d.get(1..)),
            };
            let data = data.unwrap_or(&[]);
            s.push_str(&format!(
                "[SF] ln: {len:<4} {}",
                data_text(data, true, options)
            ));
        }
        Some(1) => {
            // A first frame: twelve bits of length or, when those are
            // zero, the four bytes after.
            let short = usize::from(d[0] & 0x0F) << 8 | usize::from(d.get(1).copied().unwrap_or(0));
            let (len, data) = if short == 0 && d.len() >= 6 {
                (
                    u32::from_be_bytes([d[2], d[3], d[4], d[5]]) as usize,
                    &d[6..],
                )
            } else {
                (short, d.get(2..).unwrap_or(&[]))
            };
            s.push_str(&format!(
                "[FF] ln: {len:<4} {}",
                data_text(data, true, options)
            ));
        }
        Some(2) => {
            s.push_str(&format!(
                "[CF] sn: {:X}    {}",
                d[0] & 0x0F,
                data_text(&d[1..], false, options)
            ));
        }
        Some(3) => {
            let status = d[0] & 0x0F;
            let meaning = match status {
                0 => "CTS",
                1 => "WT",
                2 => "OVFLW",
                _ => "reserved",
            };
            let block = d.get(1).copied().unwrap_or(0);
            let block = if block == 0 {
                "0 = off".to_string()
            } else {
                block.to_string()
            };
            let st = d.get(2).copied().unwrap_or(0);
            s.push_str(&format!(
                "[FC] FC: {status} = {meaning} # BS: {block} # STmin: 0x{st:02X} = {}",
                st_min(st)
            ));
        }
        _ => s.push_str("[??]"),
    }
    if options.colour {
        let colour = match side {
            Side::Source => RED,
            Side::Destination => BLUE,
        };
        format!("{colour}{s}{RESET}")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(frame: &str, options: Options) -> String {
        let side = if frame.starts_with("7E0") || frame.starts_with("18DA10F1") {
            Side::Source
        } else {
            Side::Destination
        };
        line("", "vcan9", &frame.parse().unwrap(), side, &options)
    }

    // The lines below are what can-utils 2023.03 printed for these frames.
    #[test]
    fn frames_are_taken_apart_as_can_utils_prints_them() {
        let plain = Options::default();
        for (frame, expected) in [
            (
                "7E0#0322F190AAAAAAAA",
                " vcan9  7E0  [8]  [SF] ln: 3    data: 22 F1 90 AA AA AA AA ",
            ),
            (
                "7E8#101462F190414243",
                " vcan9  7E8  [8]  [FF] ln: 20   data: 62 F1 90 41 42 43 ",
            ),
            (
                "7E0#30080A",
                " vcan9  7E0  [3]  [FC] FC: 0 = CTS # BS: 8 # STmin: 0x0A = 10 ms",
            ),
            (
                "7E8#2145464748494A4B",
                " vcan9  7E8  [8]  [CF] sn: 1    data: 45 46 47 48 49 4A 4B ",
            ),
            (
                "7E0#3100F1",
                " vcan9  7E0  [3]  [FC] FC: 1 = WT # BS: 0 = off # STmin: 0xF1 = 100 us",
            ),
            (
                "7E0#32",
                " vcan9  7E0  [1]  [FC] FC: 2 = OVFLW # BS: 0 = off # STmin: 0x00 = 0 ms",
            ),
            (
                "7E0#037F2231",
                " vcan9  7E0  [4]  [SF] ln: 3    data: 7F 22 31 ",
            ),
            ("7E8#F0", " vcan9  7E8  [1]  [??]"),
            (
                "18DA10F1#33FFFF",
                " vcan9  18DA10F1  [3]  [FC] FC: 3 = reserved # BS: 255 # STmin: 0xFF = reserved",
            ),
            (
                "18DA10F1#3000F9",
                " vcan9  18DA10F1  [3]  [FC] FC: 0 = CTS # BS: 0 = off # STmin: 0xF9 = 900 us",
            ),
            (
                "18DA10F1#30007F",
                " vcan9  18DA10F1  [3]  [FC] FC: 0 = CTS # BS: 0 = off # STmin: 0x7F = 127 ms",
            ),
            (
                "18DA10F1#300080",
                " vcan9  18DA10F1  [3]  [FC] FC: 0 = CTS # BS: 0 = off # STmin: 0x80 = reserved",
            ),
            // The long forms of the lengths, and frames with nothing in them.
            (
                "7E0##0000A22F190AAAAAAAAAAAA",
                " vcan9  7E0 [12]  [SF] ln: 10   data: 22 F1 90 AA AA AA AA AA AA 00 ",
            ),
            (
                "7E8#1000000001006201",
                " vcan9  7E8  [8]  [FF] ln: 256  data: 62 01 ",
            ),
            ("7E0#0000", " vcan9  7E0  [2]  [SF] ln: 0    data:"),
            ("7E0#00", " vcan9  7E0  [1]  [SF] ln: 0    data:"),
        ] {
            assert_eq!(show(frame, plain), expected, "{frame}");
        }
    }

    #[test]
    fn text_names_and_colour_follow_the_data() {
        let ascii = Options {
            ascii: true,
            ..Options::default()
        };
        assert_eq!(
            show("7E0#0322F190AAAAAAAA", ascii),
            " vcan9  7E0  [8]  [SF] ln: 3    data: 22 F1 90 AA AA AA AA  -  '\"......'"
        );
        assert_eq!(
            show("7E8#101462F190414243", ascii),
            " vcan9  7E8  [8]  [FF] ln: 20   data: 62 F1 90 41 42 43     -  'b..ABC'"
        );
        assert_eq!(
            show("7E0#037F2231", ascii),
            " vcan9  7E0  [4]  [SF] ln: 3    data: 7F 22 31              -  '.\"1'"
        );
        // A flow control frame has no data to show.
        assert_eq!(
            show("7E0#30080A", ascii),
            " vcan9  7E0  [3]  [FC] FC: 0 = CTS # BS: 8 # STmin: 0x0A = 10 ms"
        );
        let uds = Options {
            uds: true,
            ..Options::default()
        };
        for (frame, expected) in [
            ("7E0#0322F190AAAAAAAA", " vcan9  7E0  [8]  [SF] ln: 3    data: 22 F1 90 AA AA AA AA  - [SRQ] ReadDataByIdentifier"),
            ("7E8#101462F190414243", " vcan9  7E8  [8]  [FF] ln: 20   data: 62 F1 90 41 42 43     - [PSR] ReadDataByIdentifier"),
            ("7E0#037F2231", " vcan9  7E0  [4]  [SF] ln: 3    data: 7F 22 31              - [NRC] requestOutOfRange"),
            ("7E0#0210", " vcan9  7E0  [2]  [SF] ln: 2    data: 10                    - [SRQ] DiagnosticSessionControl"),
            ("7E8#0250", " vcan9  7E8  [2]  [SF] ln: 2    data: 50                    - [PSR] DiagnosticSessionControl"),
            ("7E8#037F2278", " vcan9  7E8  [4]  [SF] ln: 3    data: 7F 22 78              - [NRC] requestCorrectlyReceived-ResponsePending"),
            ("7E8#037F3E12", " vcan9  7E8  [4]  [SF] ln: 3    data: 7F 3E 12              - [NRC] sub-functionNotSupported"),
            ("7E8#037F10FF", " vcan9  7E8  [4]  [SF] ln: 3    data: 7F 10 FF              - [NRC] ISOSAEReserved"),
            ("7E0#02A5", " vcan9  7E0  [2]  [SF] ln: 2    data: A5                    - [???] Unknown"),
            ("7E0#0212", " vcan9  7E0  [2]  [SF] ln: 2    data: 12                    - [SRQ] Unknown"),
            ("7E8#0252", " vcan9  7E8  [2]  [SF] ln: 2    data: 52                    - [PSR] Unknown"),
            ("7E8#037F1040", " vcan9  7E8  [4]  [SF] ln: 3    data: 7F 10 40              - [NRC] reservedByExtendedDataLinkSecurityDocument"),
            // Only the frame a message begins with says which service it is.
            ("7E8#2145464748494A4B", " vcan9  7E8  [8]  [CF] sn: 1    data: 45 46 47 48 49 4A 4B "),
        ] {
            assert_eq!(show(frame, uds), expected, "{frame}");
        }
        let colour = Options {
            colour: true,
            ..Options::default()
        };
        assert_eq!(
            show("7E0#32", colour),
            "\x1b[31m vcan9  7E0  [1]  [FC] FC: 2 = OVFLW # BS: 0 = off # STmin: 0x00 = 0 ms\x1b[0m"
        );
        assert_eq!(
            show("7E8#F0", colour),
            "\x1b[34m vcan9  7E8  [1]  [??]\x1b[0m"
        );
    }

    #[test]
    fn extended_addressing_shows_the_address_with_the_identifier() {
        let extended = Options {
            extended: true,
            ..Options::default()
        };
        assert_eq!(
            show("7E0#550322F190", extended),
            " vcan9  7E0{55}  [5]  [SF] ln: 3    data: 22 F1 90 "
        );
        assert_eq!(
            show("7E8#AA101462F19041", extended),
            " vcan9  7E8{AA}  [7]  [FF] ln: 20   data: 62 F1 90 41 "
        );
        assert_eq!(
            show("7E0#5530080A", extended),
            " vcan9  7E0{55}  [4]  [FC] FC: 0 = CTS # BS: 8 # STmin: 0x0A = 10 ms"
        );
        let both = Options {
            extended: true,
            ascii: true,
            ..Options::default()
        };
        assert_eq!(
            show("7E8#BB0150", both),
            " vcan9  7E8{BB}  [3]  [SF] ln: 1    data: 50                 -  'P'"
        );
        // With a time in front.
        let frame: Frame = "7E0#021001".parse().unwrap();
        assert_eq!(
            line(
                "(0.000000) ",
                "vcan9",
                &frame,
                Side::Source,
                &Options::default()
            ),
            "(0.000000)  vcan9  7E0  [3]  [SF] ln: 2    data: 10 01 "
        );
    }
}
