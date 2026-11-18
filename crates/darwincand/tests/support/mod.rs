// SPDX-License-Identifier: GPL-2.0-only
//! What the tests of the C library and of the Python package share.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// The path of a file of the client library, `libdarwincan.a` or
/// `libdarwincan.dylib`, built by cargo.
///
/// Cargo is asked for it. Where the libraries of a dependency lie in the
/// target directory differs between versions of cargo, so the path cannot be
/// taken from where this test is.
pub fn client_library(file: &str) -> PathBuf {
    static FILES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    let files = FILES.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let built = Command::new(cargo)
            .args(["build", "--package", "darwincan", "--message-format=json"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("cargo could not be run");
        assert!(
            built.status.success(),
            "building the client library failed: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        // Each line is a JSON object, and the files of an artifact are
        // strings in it. A path in a target directory needs no unescaping.
        String::from_utf8_lossy(&built.stdout)
            .lines()
            .filter(|line| line.contains("\"compiler-artifact\""))
            .flat_map(|line| line.split('"'))
            .filter(|text| text.contains("/libdarwincan."))
            .map(PathBuf::from)
            .collect()
    });
    let path = files
        .iter()
        .find(|path| path.file_name().is_some_and(|name| name == file))
        .unwrap_or_else(|| panic!("cargo did not build {file}"));
    assert!(path.exists(), "{} is missing", path.display());
    path.clone()
}
