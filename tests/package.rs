//! End to end on a real package, opt in: set PKG_TO_ANYPS5_TEST_PKG to the
//! path of a debug .pkg. Without it the test prints a note and passes. The
//! package is converted three times (plain, `-v` piped, `-v` in a terminal)
//! into a directory under Cargo's target tmp dir, removed afterwards.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

/// Package path to size, from `--list`.
fn listing(package: &str) -> BTreeMap<String, u64> {
    let out = run(&[package, "--list"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    text(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (size, path) = line.split_at_checked(16)?;
            Some((path.strip_prefix("  ")?.to_owned(), size.trim().parse().ok()?))
        })
        .collect()
}

/// Every file under `dir`, as a '/'-separated path relative to it, with its
/// size.
fn files_under(dir: &Path) -> BTreeMap<String, u64> {
    let mut found = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            } else {
                let relative =
                    path.strip_prefix(dir).unwrap().components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
                found.insert(relative, entry.metadata().unwrap().len());
            }
        }
    }
    found
}

/// The status line's byte format: three significant digits, binary units.
fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut value, mut unit) = (n as f64, 0);
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{n} B");
    }
    let digits = match value {
        v if v < 9.995 => format!("{v:.2}"),
        v if v < 99.95 => format!("{v:.1}"),
        v => format!("{v:.0}"),
    };
    format!("{digits} {}", UNITS[unit])
}

/// 2176 as "2,176".
fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What the summary should say per its definition: each distinct path under
/// source/ and app0/ once, at its package size (the CNT metadata, which
/// `--list` does not show, at its size on disk).
fn expected_totals(out: &Path, listed: &BTreeMap<String, u64>) -> (u64, usize) {
    let source = files_under(&out.join("source"));
    let app0 = files_under(&out.join("app0"));
    for (path, size) in &app0 {
        if let Some(listed) = listed.get(path) {
            assert_eq!(size, listed, "app0/{path} is not the size the package lists");
        }
    }
    let paths: BTreeSet<&String> = source.keys().chain(app0.keys()).collect();
    let bytes = paths.iter().map(|&path| listed.get(path).or_else(|| app0.get(path)).copied().unwrap()).sum();
    (bytes, paths.len())
}

fn convert(package: &str, out: &Path, extra: &[&str]) -> (String, String) {
    let _ = fs::remove_dir_all(out);
    let mut args = vec![package, out.to_str().unwrap()];
    args.extend_from_slice(extra);
    let result = run(&args);
    assert!(result.status.success(), "{args:?}: {}", text(&result.stderr));
    (text(&result.stdout), text(&result.stderr))
}

#[test]
fn a_real_package_converts_and_the_summary_adds_up() {
    let Some(package) = std::env::var_os("PKG_TO_ANYPS5_TEST_PKG") else {
        eprintln!("note: set PKG_TO_ANYPS5_TEST_PKG to a debug .pkg to run the end-to-end test; skipping");
        return;
    };
    let package = package.to_str().expect("UTF-8 package path").to_owned();
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{}", std::process::id()));
    let out = work.join("out");
    // Removes the output even when an assertion fails.
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(work.clone());
    let listed = listing(&package);
    assert!(listed.contains_key("eboot.bin"));

    // Without --verbose: the layout, and the old last line.
    let (stdout, _) = convert(&package, &out, &[]);
    assert!(out.join("source/eboot.bin").is_file());
    assert!(out.join("app0").is_dir() && out.join("app0/sce_sys").is_dir());
    assert!(!stdout.contains('\x1b'));
    assert!(stdout.lines().last().unwrap().starts_with("done in "), "{stdout}");
    let (bytes, files) = expected_totals(&out, &listed);

    // -v, piped: plain lines and a summary that matches the disk.
    let (stdout, stderr) = convert(&package, &out, &["-v"]);
    assert!(!stdout.contains('\x1b') && !stderr.contains('\x1b'));
    for phase in ["» [1/3] reading package", "» [2/3] unwrapping executables: ", "» [3/3] copying app0: "] {
        assert!(stdout.contains(phase), "{phase:?} in {stdout}");
    }
    assert!(!stdout.contains("done in "));
    let summary = stdout.lines().last().unwrap();
    assert_eq!(expected_totals(&out, &listed), (bytes, files), "both runs wrote the same");
    let expected = format!("✓ done  {} · {} files · ", format_bytes(bytes), format_count(files));
    assert!(summary.starts_with(&expected), "{summary:?} should start with {expected:?}");

    // -v in a terminal: frames fit and the cursor comes back.
    if Command::new("script").arg("--version").output().is_ok_and(|o| o.status.success()) {
        let _ = fs::remove_dir_all(&out);
        for cols in [100usize, 50] {
            let output = in_terminal(&format!("stty cols {cols}; exec {BIN} -v '{package}' '{}'", out.display()), &[]);
            assert!(output.starts_with("\x1b[?25l"));
            assert!(output.rfind("\x1b[?25h") > output.rfind("\x1b[?25l"), "{cols}: cursor shown at the end");
            let frames = frames(&output);
            assert!(!frames.is_empty());
            for frame in &frames {
                let shown = visible(frame.trim_end_matches('\r'));
                assert!(columns(&shown) < cols, "{cols}: {shown:?}");
            }
            let summary = last_shown_line(&output);
            assert!(summary.starts_with(&expected), "{cols}: {summary:?}");
            fs::remove_dir_all(&out).unwrap();
        }
    } else {
        eprintln!("note: util-linux `script` not found; skipping the terminal part");
    }
}
