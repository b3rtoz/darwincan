// SPDX-License-Identifier: GPL-2.0-only

//! The names of hardware interfaces, kept from one run of the daemon to
//! the next.
//!
//! An interface is named when its adapter is first seen and keeps the name
//! from then on, whatever order adapters are plugged in or found in. What
//! is remembered is the name of each channel's identity: a line of text
//! that says which adapter and which of its channels it is (model, serial
//! number, channel; or where it is plugged in, for an adapter without a
//! serial number of its own).
//!
//! The file is a list of `name = identity` lines, and may be edited while
//! the daemon is stopped.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use darwincan_core::is_valid_ifname;

#[derive(Default)]
pub(crate) struct Names {
    /// Where the names are kept; `None` remembers them for this run only.
    path: Option<PathBuf>,
    /// Name and identity, in the order they were first seen.
    entries: Vec<(String, String)>,
}

const HEADER: &str = "\
# Names of CAN interfaces, kept by darwincand so that each adapter channel
# has the same name every time. Each line is: name = which channel it is.
# Change a name with 'canctl link set <name> name <new name>', or edit
# this file while the daemon is stopped.
";

impl Names {
    /// The names kept in `path`. A file that is not there yet, or cannot
    /// be read, starts an empty list; lines that make no sense are left
    /// out.
    pub(crate) fn load(path: &Path) -> Names {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut names = Names {
            path: Some(path.to_path_buf()),
            entries: Vec::new(),
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, identity)) = line.split_once('=') else {
                continue;
            };
            let (name, identity) = (name.trim(), identity.trim());
            let known = names
                .entries
                .iter()
                .any(|(n, i)| n == name || i == identity);
            if is_valid_ifname(name) && !identity.is_empty() && !known {
                names.entries.push((name.to_string(), identity.to_string()));
            }
        }
        names
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut text = String::from(HEADER);
        for (name, identity) in &self.entries {
            // An identity is made from what a device says of itself.
            let identity: String = identity
                .chars()
                .filter(|c| !c.is_control() && *c != '=')
                .collect();
            text.push_str(&format!("{name} = {identity}\n"));
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Written beside and moved into place, so that a daemon stopped in
        // the middle leaves the old list and not half of the new one.
        let beside = path.with_extension("new");
        fs::write(&beside, text)?;
        fs::rename(&beside, path)
    }

    /// The name for the channel with this identity: the one it had
    /// before, or else the first `can<N>` that belongs to no other
    /// channel, which it then keeps. `taken` says whether an interface
    /// with a name exists right now.
    pub(crate) fn name_for(&mut self, identity: &str, taken: impl Fn(&str) -> bool) -> String {
        if let Some((name, _)) = self.entries.iter().find(|(_, i)| i == identity) {
            if !taken(name) {
                return name.clone();
            }
        }
        // Its old name has gone to something else (a virtual interface of
        // that name, say): it gets a new one.
        self.entries.retain(|(_, i)| i != identity);
        let name = (0u32..)
            .map(|n| format!("can{n}"))
            .find(|n| !taken(n) && !self.entries.iter().any(|(kept, _)| kept == n))
            .expect("some can<N> name is free");
        self.entries.push((name.clone(), identity.to_string()));
        if let Err(e) = self.save() {
            crate::backends::log(format!(
                "cannot keep the name {name} for next time ({e}); \
                 it may be another after a restart"
            ));
        }
        name
    }

    /// Whether `name` is kept for a channel other than `identity`.
    pub(crate) fn belongs_to_another(&self, name: &str, identity: Option<&str>) -> bool {
        self.entries
            .iter()
            .any(|(n, i)| n == name && Some(i.as_str()) != identity)
    }

    /// Keep `name` for the channel with this identity from now on.
    pub(crate) fn rename(&mut self, identity: &str, name: &str) -> io::Result<()> {
        self.entries.retain(|(_, i)| i != identity);
        self.entries.push((name.to_string(), identity.to_string()));
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("darwincan-names-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("interfaces")
    }

    #[test]
    fn a_channel_keeps_its_name_whatever_the_order() {
        let path = scratch("order");
        let mut first = Names::load(&path);
        assert_eq!(first.name_for("adapter A", none), "can0");
        assert_eq!(first.name_for("adapter B channel 1", none), "can1");
        assert_eq!(first.name_for("adapter B channel 2", none), "can2");
        assert_eq!(first.name_for("adapter A", none), "can0");

        // The next run finds them the other way round.
        let mut second = Names::load(&path);
        assert_eq!(second.name_for("adapter B channel 2", none), "can2");
        assert_eq!(second.name_for("adapter B channel 1", none), "can1");
        // One not seen before does not take the name of one that is away.
        assert_eq!(second.name_for("adapter C", none), "can3");
        assert_eq!(second.name_for("adapter A", none), "can0");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_new_name_is_kept_and_frees_the_old_one() {
        let path = scratch("rename");
        let mut names = Names::load(&path);
        names.name_for("adapter A", none);
        names.name_for("adapter B", none);
        names.rename("adapter A", "chassis").unwrap();
        assert!(names.belongs_to_another("chassis", Some("adapter B")));
        assert!(!names.belongs_to_another("chassis", Some("adapter A")));
        assert!(!names.belongs_to_another("can0", None));

        let mut again = Names::load(&path);
        assert_eq!(again.name_for("adapter A", none), "chassis");
        assert_eq!(again.name_for("adapter B", none), "can1");
        assert_eq!(again.name_for("adapter C", none), "can0");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_name_in_use_by_something_else_is_not_given_twice() {
        let mut names = Names::default();
        assert_eq!(names.name_for("adapter A", none), "can0");
        // A virtual interface has been called can0 since.
        let taken = |name: &str| name == "can0";
        assert_eq!(names.name_for("adapter A", taken), "can1");
        assert_eq!(names.name_for("adapter A", taken), "can1");
    }

    #[test]
    fn a_file_edited_by_hand_is_read_with_care() {
        let path = scratch("edited");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# a comment\n\n  body = adapter A  \nnot a line\nany = adapter B\n\
             body = adapter C\nrear = adapter A\npowertrain=adapter D\n",
        )
        .unwrap();
        let mut names = Names::load(&path);
        assert_eq!(names.name_for("adapter A", none), "body");
        // "any" is no name, and a name or a channel listed twice counts
        // the first time only.
        assert_eq!(names.name_for("adapter B", none), "can0");
        assert_eq!(names.name_for("adapter C", none), "can1");
        assert_eq!(names.name_for("adapter D", none), "powertrain");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
