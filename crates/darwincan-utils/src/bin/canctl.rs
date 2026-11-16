// SPDX-License-Identifier: MIT OR Apache-2.0

use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use darwincan::{BusConfig, Control, Frame, Id, IfaceInfo, RawSocket};
use darwincan_utils::{fail, usage};

const TOOL: &str = "canctl";
const USAGE: &str = "\
Usage: canctl [link] <command>

Manage the daemon's CAN interfaces. Commands follow 'ip link' on Linux:
replace 'ip' with 'canctl' and the usual command lines work.

Commands:
  show [<interface>]         List the interfaces (also: list)
  set <interface> [up|down] [name <new name>] [type can <settings>]
                             Configure a hardware interface and/or put it on
                             or take it off the bus. Settings can only be
                             changed while it is down; they are remembered,
                             so a later 'set <interface> up' reuses them.
                             'name' calls the interface something else from
                             now on; a hardware interface has to be down,
                             and its adapter keeps the name.
  add <name> type vcan       Create a virtual CAN interface
  delete <name>              Remove a virtual CAN interface (also: del)
  latency <interface> [n]    Measure delivery latency between two clients on
                             this machine over n frames (default 2000)

Settings:
  bitrate <bps>              Nominal bitrate
  dbitrate <bps>             CAN FD data bitrate; enables CAN FD
  sample-point <0.xxx>       Nominal sample point (default: by bitrate)
  dsample-point <0.xxx>      Data sample point
  listen-only [on|off]       Never transmit or acknowledge
  fd [on|off]                'on' needs dbitrate; 'off' drops it
The word 'dev' before an interface name is optional. 'restart-ms 0' and
'off' for loopback, one-shot, triple-sampling, fd-non-iso and presume-ack
are accepted because they are the only behaviour available.

Short forms:
  up <interface> [<settings>]    = set <interface> up type can <settings>
  down <interface>               = set <interface> down
  add-vcan <name>                = add <name> type vcan

Examples:
  canctl link set can0 up type can bitrate 500000
  canctl link set can0 type can bitrate 500000 dbitrate 2000000 fd on
  canctl link set can0 up
  canctl up can0 bitrate 250000 listen-only
  canctl link add dev vcan1 type vcan
  canctl link show";

/// Bus settings given on the command line; what is absent keeps its
/// stored value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ConfigPatch {
    bitrate: Option<u32>,
    data_bitrate: Option<u32>,
    sample_point: Option<u16>,
    data_sample_point: Option<u16>,
    listen_only: Option<bool>,
    fd: Option<bool>,
}

impl ConfigPatch {
    /// The configuration that results from applying these settings to the
    /// interface's stored one.
    fn apply(&self, stored: Option<BusConfig>) -> Result<BusConfig, String> {
        let mut c = stored.unwrap_or(BusConfig::new(0));
        if let Some(b) = self.bitrate {
            c.bitrate = b;
        }
        if self.data_bitrate.is_some() {
            c.data_bitrate = self.data_bitrate;
        }
        if self.sample_point.is_some() {
            c.sample_point = self.sample_point;
        }
        if self.data_sample_point.is_some() {
            c.data_sample_point = self.data_sample_point;
        }
        if let Some(l) = self.listen_only {
            c.listen_only = l;
        }
        match self.fd {
            Some(true) if c.data_bitrate.is_none() => {
                return Err("fd on needs dbitrate <bps>".into())
            }
            Some(false) => {
                c.data_bitrate = None;
                c.data_sample_point = None;
            }
            _ => {}
        }
        if c.bitrate == 0 {
            return Err("missing bitrate <bps>".into());
        }
        Ok(c)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Show(Option<String>),
    Set {
        ifname: String,
        /// `Some(true)` for up, `Some(false)` for down.
        state: Option<bool>,
        patch: Option<ConfigPatch>,
        /// What the interface is to be called from now on.
        rename: Option<String>,
    },
    AddVcan(String),
    Delete(String),
    Latency(String, usize),
    Help,
}

fn on_off(v: &str, key: &str) -> Result<bool, String> {
    match v {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("{key} takes on or off, not {v}")),
    }
}

/// Parse the settings that follow `type can` (or `canctl up <interface>`).
fn parse_settings(options: &[&str]) -> Result<ConfigPatch, String> {
    let mut patch = ConfigPatch::default();
    let mut it = options.iter().copied().peekable();
    while let Some(key) = it.next() {
        let rate = |v: &str| {
            v.parse::<u32>()
                .ok()
                .filter(|r| *r > 0)
                .ok_or(format!("invalid bitrate: {v}"))
        };
        // Sample points are given as a fraction, e.g. 0.875.
        let point = |v: &str| {
            v.parse::<f64>()
                .ok()
                .filter(|p| (0.5..=0.95).contains(p))
                .map(|p| (p * 1000.0).round() as u16)
                .ok_or(format!("invalid sample point: {v} (expected 0.5 to 0.95)"))
        };
        let mut value = || it.next().ok_or(format!("{key} needs a value"));
        match key {
            "bitrate" => patch.bitrate = Some(rate(value()?)?),
            "dbitrate" => patch.data_bitrate = Some(rate(value()?)?),
            "sample-point" => patch.sample_point = Some(point(value()?)?),
            "dsample-point" => patch.data_sample_point = Some(point(value()?)?),
            "fd" => patch.fd = Some(on_off(value()?, key)?),
            "listen-only" => {
                // Linux wants on/off; the bare word is this tool's short form.
                patch.listen_only = Some(match it.next_if(|v| matches!(*v, "on" | "off")) {
                    Some(v) => v == "on",
                    None => true,
                });
            }
            "loopback" | "one-shot" | "triple-sampling" | "fd-non-iso" | "presume-ack"
            | "cc-len8-dlc" => {
                if on_off(value()?, key)? {
                    return Err(format!("{key} on is not supported"));
                }
            }
            "berr-reporting" => {
                // Bus errors are always reported; see candump -e.
                on_off(value()?, key)?;
            }
            "restart-ms" => {
                if value()? != "0" {
                    return Err("automatic restart after bus off is not supported yet".into());
                }
            }
            "tq" | "prop-seg" | "phase-seg1" | "phase-seg2" | "sjw" | "dtq" | "dprop-seg"
            | "dphase-seg1" | "dphase-seg2" | "dsjw" => {
                return Err(format!(
                    "{key} is not supported; give bitrate and sample-point instead"
                ));
            }
            other => return Err(format!("unknown setting: {other}")),
        }
    }
    Ok(patch)
}

/// Parse the arguments after `set`: `[dev] <interface>`, `up`/`down` and
/// `type can <settings>` in any order, the settings last.
fn parse_set(args: &[&str]) -> Result<Command, String> {
    let mut ifname = None;
    let mut state = None;
    let mut patch = None;
    let mut rename = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "up" => state = Some(true),
            "down" => state = Some(false),
            "dev" => {}
            "name" => {
                match args.get(i + 1) {
                    Some(new) => rename = Some(new.to_string()),
                    None => return Err("name needs a value".into()),
                }
                i += 1;
            }
            "type" => {
                match args.get(i + 1) {
                    Some(&"can") => {}
                    Some(other) => return Err(format!("type {other} has no settings")),
                    None => return Err("type needs a value".into()),
                }
                patch = Some(parse_settings(&args[i + 2..])?);
                break;
            }
            // Accepted for scripts written for Linux; nothing to adjust here.
            "txqueuelen" | "mtu" => {
                if args.get(i + 1).is_none() {
                    return Err(format!("{} needs a value", args[i]));
                }
                i += 1;
            }
            name => {
                if ifname.replace(name.to_string()).is_some() {
                    return Err(format!("unexpected argument: {name}"));
                }
            }
        }
        i += 1;
    }
    Ok(Command::Set {
        ifname: ifname.ok_or("missing interface name")?,
        state,
        patch,
        rename,
    })
}

/// An interface name, optionally preceded by `dev` (or `name` for `add`).
fn dev_name<'a>(args: &'a [&'a str]) -> Option<(&'a str, &'a [&'a str])> {
    match args {
        ["dev" | "name", name, rest @ ..] | [name, rest @ ..] => Some((*name, rest)),
        [] => None,
    }
}

fn parse_command(args: &[&str]) -> Result<Command, String> {
    // Display options of `ip` (-details, -s, -br, ...) change nothing here.
    let skip = args
        .iter()
        .take_while(|a| a.starts_with('-') && !matches!(**a, "-h" | "--help"))
        .count();
    let args = &args[skip..];
    let args = match args {
        ["link", rest @ ..] => rest,
        other => other,
    };
    match args {
        ["-h" | "--help" | "help", ..] => Ok(Command::Help),
        ["show" | "list" | "ls" | "sh"] => Ok(Command::Show(None)),
        ["show" | "list" | "ls" | "sh", rest @ ..] => match dev_name(rest) {
            Some((name, [])) => Ok(Command::Show(Some(name.to_string()))),
            _ => Err("show takes at most one interface".into()),
        },
        ["set", rest @ ..] => parse_set(rest),
        ["up", ifname, settings @ ..] => Ok(Command::Set {
            ifname: ifname.to_string(),
            state: Some(true),
            patch: match settings {
                [] => None,
                _ => Some(parse_settings(settings)?),
            },
            rename: None,
        }),
        ["down", ifname] => Ok(Command::Set {
            ifname: ifname.to_string(),
            state: Some(false),
            patch: None,
            rename: None,
        }),
        ["add-vcan", name] => Ok(Command::AddVcan(name.to_string())),
        ["add", rest @ ..] => match dev_name(rest) {
            Some((name, ["type", "vcan"])) => Ok(Command::AddVcan(name.to_string())),
            Some((_, ["type", other])) => Err(format!(
                "cannot add interfaces of type {other}; only vcan (hardware \
                 interfaces appear when an adapter is plugged in)"
            )),
            _ => Err("expected: add <name> type vcan".into()),
        },
        ["delete" | "del", rest @ ..] => match dev_name(rest) {
            Some((name, [] | ["type", "vcan"])) => Ok(Command::Delete(name.to_string())),
            _ => Err("expected: delete <name>".into()),
        },
        ["latency", ifname] => Ok(Command::Latency(ifname.to_string(), 2000)),
        ["latency", ifname, n] => match n.parse() {
            Ok(n) if n > 0 => Ok(Command::Latency(ifname.to_string(), n)),
            _ => Err(format!("invalid frame count: {n}")),
        },
        _ => Err("unknown or incomplete command".into()),
    }
}

/// What to ask of the daemon for a `set` command, given the interface's
/// current state.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Up(BusConfig),
    Down,
    Configure(BusConfig),
}

fn plan_set(
    iface: &IfaceInfo,
    state: Option<bool>,
    patch: Option<ConfigPatch>,
) -> Result<Vec<Step>, String> {
    let name = &iface.name;
    if iface.kind == "virtual" {
        return match (state, patch) {
            (_, Some(_)) => Err(format!("{name} is virtual and has no bus settings")),
            (Some(false), None) => Err(format!("{name} is virtual and always up")),
            // Linux needs `ip link set vcan0 up` after creating it; here it
            // already is.
            _ => Ok(Vec::new()),
        };
    }
    let config = patch.map(|p| p.apply(iface.config)).transpose()?;
    match (state, config) {
        (Some(true), Some(c)) => Ok(vec![Step::Up(c)]),
        (Some(true), None) if iface.up => Ok(Vec::new()),
        (Some(true), None) => match iface.config {
            Some(c) => Ok(vec![Step::Up(c)]),
            None => Err(format!(
                "{name} has no bitrate yet; use: canctl link set {name} up type can bitrate <bps>"
            )),
        },
        (Some(false), Some(c)) => Ok(vec![Step::Down, Step::Configure(c)]),
        (Some(false), None) => Ok(vec![Step::Down]),
        (None, Some(c)) => Ok(vec![Step::Configure(c)]),
        (None, None) => Err("nothing to do: give up, down or type can <settings>".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let command = match parse_command(&args) {
        Ok(c) => c,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let result = match command {
        Command::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Command::Show(name) => show(name.as_deref()),
        Command::Set {
            ifname,
            state,
            patch,
            rename,
        } => set(&ifname, state, patch, rename),
        Command::AddVcan(name) => text(Control::connect().and_then(|c| c.add_vcan(&name))),
        Command::Delete(name) => text(Control::connect().and_then(|c| c.del_vcan(&name))),
        Command::Latency(ifname, n) => text(latency(&ifname, n)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}

fn text<T>(result: darwincan::Result<T>) -> Result<T, String> {
    result.map_err(|e| e.to_string())
}

fn find(ctl: &Control, name: &str) -> Result<IfaceInfo, String> {
    text(ctl.interfaces())?
        .into_iter()
        .find(|i| i.name == name)
        .ok_or(format!("no such interface: {name}"))
}

fn set(
    ifname: &str,
    state: Option<bool>,
    patch: Option<ConfigPatch>,
    rename: Option<String>,
) -> Result<(), String> {
    let ctl = text(Control::connect())?;
    let iface = find(&ctl, ifname)?;
    // A new name alone is something to do, with no steps besides.
    let steps = if rename.is_some() && state.is_none() && patch.is_none() {
        Vec::new()
    } else {
        plan_set(&iface, state, patch)?
    };
    // An interface is renamed while it is down: before it goes up, or
    // after it has come down.
    let mut name = ifname.to_string();
    let mut rename = rename;
    if state == Some(true) {
        if let Some(new) = rename.take() {
            text(ctl.rename(&name, &new))?;
            name = new;
        }
    }
    for step in steps {
        text(match step {
            Step::Up(config) => ctl.up(&name, &config),
            Step::Down => ctl.down(&name),
            Step::Configure(config) => ctl.configure(&name, &config),
        })?;
    }
    if let Some(new) = rename {
        text(ctl.rename(&name, &new))?;
    }
    Ok(())
}

fn describe(iface: &IfaceInfo) -> String {
    let settings = iface.config.map(|c| {
        let mut s = format!("{} bit/s", c.bitrate);
        if let Some(d) = c.data_bitrate {
            s.push_str(&format!(", data {d} bit/s"));
        }
        if c.listen_only {
            s.push_str(", listen-only");
        }
        s
    });
    match (iface.up, settings) {
        (true, Some(s)) => format!("up, {s}"),
        (true, None) => "up".to_string(),
        (false, Some(s)) => format!("down ({s})"),
        (false, None) => "down".to_string(),
    }
}

fn show(only: Option<&str>) -> Result<(), String> {
    let ctl = text(Control::connect())?;
    let list = match only {
        Some(name) => vec![find(&ctl, name)?],
        None => text(ctl.interfaces())?,
    };
    for iface in list {
        print!("{:<8} {:<10} {}", iface.name, iface.kind, describe(&iface));
        if !iface.description.is_empty() {
            print!("  [{}]", iface.description);
        }
        println!();
    }
    Ok(())
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Send frames from one socket to another through the daemon and report
/// how long each took, client to client.
///
/// On a hardware interface the test frames go out on the wire, so only run
/// this where id 0x7FF is harmless.
fn latency(ifname: &str, count: usize) -> darwincan::Result<()> {
    let tx = RawSocket::open(ifname)?;
    let rx = Arc::new(RawSocket::open(ifname)?);
    rx.set_read_timeout(Some(Duration::from_secs(2)))?;

    let epoch = Instant::now();
    let receiver = {
        let rx = rx.clone();
        thread::spawn(move || -> darwincan::Result<Vec<(u64, u64)>> {
            let mut seen = Vec::with_capacity(count);
            while seen.len() < count {
                let frame = rx.recv()?.frame;
                let arrived = epoch.elapsed().as_nanos() as u64;
                if frame.id() == Id::Standard(0x7FF) && frame.len() == 8 {
                    let sent = u64::from_le_bytes(frame.data().try_into().unwrap());
                    seen.push((sent, arrived));
                }
            }
            Ok(seen)
        })
    };

    for _ in 0..count {
        let sent = epoch.elapsed().as_nanos() as u64;
        let frame = Frame::new(Id::Standard(0x7FF), &sent.to_le_bytes())
            .expect("8 bytes fit a classic frame");
        tx.send(&frame)?;
        // Space frames out so each measurement stands alone.
        thread::sleep(Duration::from_micros(500));
    }

    let seen = receiver.join().expect("receiver thread panicked")?;
    let mut lat: Vec<u64> = seen.iter().map(|(s, a)| a.saturating_sub(*s)).collect();
    lat.sort_unstable();
    let us = |ns: u64| ns as f64 / 1000.0;
    println!("{count} frames on {ifname}, client -> daemon -> client:");
    println!("  min    {:>8.1} us", us(lat[0]));
    println!("  median {:>8.1} us", us(percentile(&lat, 0.50)));
    println!("  p99    {:>8.1} us", us(percentile(&lat, 0.99)));
    println!("  max    {:>8.1} us", us(*lat.last().unwrap()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Command, String> {
        parse_command(&line.split_whitespace().collect::<Vec<_>>())
    }

    fn set(ifname: &str, state: Option<bool>, patch: Option<ConfigPatch>) -> Command {
        Command::Set {
            ifname: ifname.into(),
            state,
            patch,
            rename: None,
        }
    }

    fn bitrate(bps: u32) -> ConfigPatch {
        ConfigPatch {
            bitrate: Some(bps),
            ..ConfigPatch::default()
        }
    }

    #[test]
    fn settings() {
        assert_eq!(parse_settings(&["bitrate", "500000"]), Ok(bitrate(500_000)));
        let full = ConfigPatch {
            bitrate: Some(500_000),
            data_bitrate: Some(2_000_000),
            sample_point: Some(875),
            data_sample_point: Some(800),
            listen_only: Some(true),
            fd: Some(true),
        };
        for line in [
            "bitrate 500000 dbitrate 2000000 sample-point 0.875 dsample-point 0.8 listen-only fd on",
            "bitrate 500000 sample-point 0.875 dbitrate 2000000 dsample-point 0.8 fd on listen-only on",
            "bitrate 500000 dbitrate 2000000 sample-point 0.875 dsample-point 0.8 listen-only on \
             fd on restart-ms 0 loopback off berr-reporting on one-shot off",
        ] {
            let words: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(parse_settings(&words), Ok(full), "{line}");
        }
        assert_eq!(
            parse_settings(&["listen-only", "off"]),
            Ok(ConfigPatch {
                listen_only: Some(false),
                ..ConfigPatch::default()
            })
        );

        for bad in [
            "bitrate",
            "bitrate 0",
            "bitrate 500k",
            "bitrate 500000 sample-point 87.5",
            "bitrate 500000 loopback on",
            "bitrate 500000 loopback",
            "bitrate 500000 fd maybe",
            "bitrate 500000 restart-ms 100",
            "bitrate 500000 tq 125",
            "bitrate 500000 frobnicate",
        ] {
            let words: Vec<&str> = bad.split_whitespace().collect();
            assert!(parse_settings(&words).is_err(), "{bad}");
        }
    }

    #[test]
    fn patches_apply_to_the_stored_configuration() {
        assert_eq!(bitrate(500_000).apply(None), Ok(BusConfig::new(500_000)));
        // Without a bitrate there must be a stored one to build on.
        let listen = ConfigPatch {
            listen_only: Some(true),
            ..ConfigPatch::default()
        };
        assert!(listen.apply(None).is_err());
        assert_eq!(
            listen.apply(Some(BusConfig::fd(500_000, 2_000_000))),
            Ok(BusConfig {
                listen_only: true,
                ..BusConfig::fd(500_000, 2_000_000)
            })
        );
        // fd on needs a data bitrate from somewhere; fd off drops it.
        let fd = |on| ConfigPatch {
            fd: Some(on),
            ..bitrate(250_000)
        };
        assert!(fd(true).apply(None).is_err());
        assert_eq!(
            fd(true).apply(Some(BusConfig::fd(500_000, 2_000_000))),
            Ok(BusConfig::fd(250_000, 2_000_000))
        );
        assert_eq!(
            fd(false).apply(Some(BusConfig::fd(500_000, 2_000_000))),
            Ok(BusConfig::new(250_000))
        );
    }

    #[test]
    fn ip_link_command_lines() {
        let up_500k = set("can0", Some(true), Some(bitrate(500_000)));
        for line in [
            "link set can0 up type can bitrate 500000",
            "link set dev can0 up type can bitrate 500000",
            "link set up can0 type can bitrate 500000",
            "-details link set can0 up type can bitrate 500000",
            "set can0 up type can bitrate 500000",
            "link set can0 txqueuelen 1000 up type can bitrate 500000",
            "up can0 bitrate 500000",
        ] {
            assert_eq!(parse(line).as_ref(), Ok(&up_500k), "{line}");
        }
        assert_eq!(
            parse("link set can0 down name chassis"),
            Ok(Command::Set {
                ifname: "can0".into(),
                state: Some(false),
                patch: None,
                rename: Some("chassis".into()),
            })
        );
        assert_eq!(
            parse("link set dev can0 name chassis"),
            Ok(Command::Set {
                ifname: "can0".into(),
                state: None,
                patch: None,
                rename: Some("chassis".into()),
            })
        );
        assert!(parse("link set can0 name").is_err());
        // Settings run to the end of the line, as with ip.
        assert!(parse("link set can0 type can bitrate 500000 up").is_err());

        assert_eq!(
            parse("link set can0 type can bitrate 250000 listen-only on"),
            Ok(set(
                "can0",
                None,
                Some(ConfigPatch {
                    listen_only: Some(true),
                    ..bitrate(250_000)
                })
            ))
        );
        assert_eq!(parse("link set can0 up"), Ok(set("can0", Some(true), None)));
        assert_eq!(
            parse("link set up dev can0"),
            Ok(set("can0", Some(true), None))
        );
        assert_eq!(
            parse("link set can0 down"),
            Ok(set("can0", Some(false), None))
        );
        assert_eq!(parse("down can0"), Ok(set("can0", Some(false), None)));
        assert_eq!(parse("up can0"), Ok(set("can0", Some(true), None)));
        assert_eq!(parse("link set vcan0 mtu 72"), Ok(set("vcan0", None, None)));

        for line in [
            "link add dev vcan1 type vcan",
            "link add vcan1 type vcan",
            "link add name vcan1 type vcan",
            "add-vcan vcan1",
        ] {
            assert_eq!(parse(line), Ok(Command::AddVcan("vcan1".into())), "{line}");
        }
        assert!(parse("link add dev can5 type can").is_err());
        assert!(parse("link add dev vcan1").is_err());
        for line in [
            "link delete vcan1",
            "link del dev vcan1",
            "delete vcan1 type vcan",
        ] {
            assert_eq!(parse(line), Ok(Command::Delete("vcan1".into())), "{line}");
        }

        for line in [
            "link show",
            "list",
            "-details -statistics link show",
            "link ls",
        ] {
            assert_eq!(parse(line), Ok(Command::Show(None)), "{line}");
        }
        assert_eq!(
            parse("link show can0"),
            Ok(Command::Show(Some("can0".into())))
        );
        assert_eq!(
            parse("-d link show dev can0"),
            Ok(Command::Show(Some("can0".into())))
        );

        assert_eq!(
            parse("latency vcan0"),
            Ok(Command::Latency("vcan0".into(), 2000))
        );
        assert_eq!(
            parse("latency vcan0 50"),
            Ok(Command::Latency("vcan0".into(), 50))
        );
        assert_eq!(parse("--help"), Ok(Command::Help));
        for bad in [
            "",
            "link",
            "link set",
            "link set up",
            "link set can0 can1 up",
            "link set can0 type vcan",
            "link set can0 type",
            "latency vcan0 0",
            "frobnicate",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    fn iface(kind: &str, up: bool, config: Option<BusConfig>) -> IfaceInfo {
        IfaceInfo {
            index: 1,
            name: "can0".into(),
            kind: kind.into(),
            description: String::new(),
            up,
            config,
        }
    }

    #[test]
    fn set_plans_follow_linux() {
        let c500 = BusConfig::new(500_000);
        let c250 = BusConfig::new(250_000);
        let p250 = Some(bitrate(250_000));

        // Configure and go up in one command, or in two.
        let fresh = iface("peak_fd", false, None);
        assert_eq!(plan_set(&fresh, Some(true), p250), Ok(vec![Step::Up(c250)]));
        assert_eq!(
            plan_set(&fresh, None, p250),
            Ok(vec![Step::Configure(c250)])
        );
        assert!(plan_set(&fresh, Some(true), None).is_err());
        assert!(plan_set(&fresh, None, None).is_err());

        let configured = iface("peak_fd", false, Some(c500));
        assert_eq!(
            plan_set(&configured, Some(true), None),
            Ok(vec![Step::Up(c500)])
        );
        assert_eq!(
            plan_set(&configured, Some(false), None),
            Ok(vec![Step::Down])
        );
        // A setting on its own builds on what is stored.
        let listen = Some(ConfigPatch {
            listen_only: Some(true),
            ..ConfigPatch::default()
        });
        assert_eq!(
            plan_set(&configured, None, listen),
            Ok(vec![Step::Configure(BusConfig {
                listen_only: true,
                ..c500
            })])
        );

        let running = iface("peak_fd", true, Some(c500));
        assert_eq!(plan_set(&running, Some(true), None), Ok(vec![]));
        assert_eq!(
            plan_set(&running, Some(true), p250),
            Ok(vec![Step::Up(c250)])
        );
        assert_eq!(
            plan_set(&running, Some(false), p250),
            Ok(vec![Step::Down, Step::Configure(c250)])
        );
        // Left to the daemon, which refuses to reconfigure a running interface.
        assert_eq!(
            plan_set(&running, None, p250),
            Ok(vec![Step::Configure(c250)])
        );

        let vcan = iface("virtual", true, None);
        assert_eq!(plan_set(&vcan, Some(true), None), Ok(vec![]));
        assert_eq!(plan_set(&vcan, None, None), Ok(vec![]));
        assert!(plan_set(&vcan, Some(false), None).is_err());
        assert!(plan_set(&vcan, Some(true), p250).is_err());
    }

    #[test]
    fn descriptions() {
        assert_eq!(describe(&iface("virtual", true, None)), "up");
        assert_eq!(describe(&iface("peak_fd", false, None)), "down");
        assert_eq!(
            describe(&iface("peak_fd", false, Some(BusConfig::new(500_000)))),
            "down (500000 bit/s)"
        );
        assert_eq!(
            describe(&iface(
                "peak_fd",
                true,
                Some(BusConfig {
                    listen_only: true,
                    ..BusConfig::fd(500_000, 2_000_000)
                })
            )),
            "up, 500000 bit/s, data 2000000 bit/s, listen-only"
        );
    }
}
