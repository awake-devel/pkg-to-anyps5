//! Runs the built binary: arguments, the output without `--verbose`, the
//! plain `--verbose` output, and the live status line in a real terminal
//! through `script` (skipped with a note where `script` is missing).

mod common;

use std::fs;
use std::process::Command;

use common::*;

/// Sizes of the files the demo writes, in MiB: unwrapped executables, then
/// the app0/ copies (which repeat one executable).
type Files = Vec<(String, u64)>;

fn demo_files() -> (Files, Files) {
    let executables = [
        ("eboot.bin", 46),
        ("sce_module/libc.prx", 3),
        ("sce_module/libSceFios2.prx", 1),
        ("Media/Modules/GameAssembly.prx", 92),
        ("Media/Modules/Il2cppUserAssemblies.prx", 38),
        ("Media/Plugins/libfmod.prx", 6),
    ];
    let mut copies: Vec<(String, u64)> = (0..40).map(|i| (format!("chunk_{i:03}"), 13 + i * 7 % 29)).collect();
    for (name, size) in [("level", 520), ("resources", 310), ("level0", 180), ("日本語", 24), ("Media/Modules/GameAssembly.prx", 92)] {
        copies.push((name.to_owned(), size));
    }
    (executables.iter().map(|&(n, s)| (n.to_owned(), s)).collect(), copies)
}

#[test]
fn help_lists_the_verbose_flag() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let help = text(&out.stdout);
    assert!(help.contains("  -v, --verbose        show a live status line"), "{help}");
    assert!(!help.contains("status-demo"), "the demo stays hidden");
    assert_eq!(text(&run(&["-h"]).stdout), help);
}

#[test]
fn relink_is_marked_deprecated() {
    let help = text(&run(&["--help"]).stdout);
    assert_eq!(help.matches("removed in v1").count(), 3, "--relink, --module-dir and --windows: {help}");

    let dir = scratch("relink");
    let missing = dir.join("missing.pkg");
    let out = run(&[missing.to_str().unwrap(), dir.join("out").to_str().unwrap(), "--relink", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    let (warning, error) = stderr.split_once('\n').unwrap();
    assert!(warning.starts_with("warning: --relink") && warning.contains("removed in v1"), "{stderr}");
    assert!(error.starts_with("error: open "), "the run still goes ahead: {stderr}");
    assert!(out.stdout.is_empty(), "the warning stays off stdout");

    // No warning without --relink.
    let plain = run(&[missing.to_str().unwrap(), dir.join("out").to_str().unwrap()]);
    assert!(!text(&plain.stderr).contains("warning"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn version() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert_eq!(text(&out.stdout), format!("pkg-to-anyps5 {}\n", env!("CARGO_PKG_VERSION")));
    assert_eq!(text(&run(&["-V"]).stdout), text(&out.stdout));
}

#[test]
fn bad_arguments_exit_1_with_the_usage() {
    for (args, message) in [
        (&["--bogus"][..], "unknown option --bogus"),
        (&[][..], "missing the package path"),
        (&["game.pkg"][..], "missing the output directory"),
        (&["game.pkg", "out", "extra"][..], "too many arguments"),
        (&["game.pkg", "out", "--windows"][..], "--windows only applies with --relink"),
        (&["game.pkg", "out", "--jobs", "0"][..], "--jobs needs a positive number"),
        // The demo only runs on its own.
        (&["--status-demo", "game.pkg"][..], "unknown option --status-demo"),
        (&["game.pkg", "out", "-v", "--status-demo"][..], "unknown option --status-demo"),
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = text(&out.stderr);
        assert!(stderr.starts_with(&format!("error: {message}\n\nusage: pkg-to-anyps5 ")), "{args:?}: {stderr}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn errors_without_verbose_print_what_they_always_did() {
    let dir = scratch("errors");
    let missing = dir.join("missing.pkg");
    let short = dir.join("short.bin");
    fs::write(&short, b"hello world, not a package").unwrap();
    let wrong = dir.join("wrong.bin");
    fs::write(&wrong, [b'A'; 512]).unwrap();
    for (package, expected) in [
        // The OS words "not found" its own way (Windows differs from unix).
        (&missing, format!("error: open {}: {}\n", missing.display(), fs::File::open(&missing).unwrap_err())),
        (&short, "error: invalid package: read of 0x100 bytes at 0x0 is past the end of the 0x1a-byte package\n".to_owned()),
        (&wrong, "error: invalid package: not a PS5 package: magic [41, 41, 41, 41]\n".to_owned()),
    ] {
        let out = run(&[package.to_str().unwrap(), dir.join("out").to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(text(&out.stderr), expected);
        assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
        let list = run(&[package.to_str().unwrap(), "--list"]);
        assert_eq!(text(&list.stderr), expected);
        assert!(list.stdout.is_empty());
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn verbose_errors_without_a_terminal_say_where_they_stopped() {
    let dir = scratch("verbose-errors");
    let short = dir.join("short.bin");
    fs::write(&short, b"hello world, not a package").unwrap();
    let out = run(&["-v", short.to_str().unwrap(), dir.join("out").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let stdout = text(&out.stdout);
    assert!(stdout.starts_with("» [1/3] reading package\n✗ stopped while reading package · "), "{stdout}");
    assert!(!stdout.contains('\x1b'));
    assert!(text(&out.stderr).starts_with("error: invalid package: "));
    fs::remove_dir_all(&dir).unwrap();
}

/// What the plain `--verbose` output must hold.
fn assert_plain_demo(output: &str) {
    assert!(!output.contains('\x1b'), "escape codes in plain output: {output:?}");
    assert!(!output.contains('\x18'));
    let order = [
        "» [1/4] reading package",
        "package     demo.pkg",
        "» [2/4] unwrapping executables: 6 files, 186.0 MiB",
        "  source/eboot.bin ",
        "» [3/4] copying app0: 45 files, ",
        "  app0/  45 files, ",
        "» [4/4] relinking",
        "  out/app.elf and libs/ (41 system libraries) are ready",
        "✓ done  ",
    ];
    let mut at = 0;
    for part in order {
        let found = output[at..].find(part).unwrap_or_else(|| panic!("{part:?} missing or out of order in {output}"));
        at += found + part.len();
    }
    // A plain progress line, every 5 s while copying.
    assert!(output.contains("  copying app0  "), "{output}");
}

#[test]
fn verbose_with_stdout_piped_prints_plain_lines() {
    let out = run(&["--status-demo"]);
    assert!(out.status.success());
    let stdout = text(&out.stdout);
    assert_plain_demo(&stdout);

    // Every package file once: GameAssembly.prx was unwrapped and copied.
    let (executables, copies) = demo_files();
    let mut seen = std::collections::HashMap::new();
    for (name, size) in executables.iter().chain(&copies) {
        seen.insert(name.clone(), *size);
    }
    let mib: u64 = seen.values().sum();
    assert_eq!(seen.len(), 50);
    let summary = stdout.lines().last().unwrap();
    let expected = format!("✓ done  {:.2} GiB · 50 files · ", mib as f64 / 1024.0);
    assert!(summary.starts_with(&expected), "{summary:?} should start with {expected:?}");
    assert!(summary.contains(" · avg ") && summary.ends_with("/s"), "{summary:?}");
}

#[test]
fn verbose_with_no_color_or_a_dumb_terminal_prints_plain_lines() {
    if !has_script() {
        return;
    }
    for env in [("NO_COLOR", "1"), ("TERM", "dumb")] {
        let output = in_terminal(&format!("stty cols 100; exec {BIN} --status-demo"), &[env]);
        assert_plain_demo(&output.replace("\r\n", "\n"));
    }
}

#[test]
fn the_live_line_fits_every_width_and_gives_the_cursor_back() {
    if !has_script() {
        return;
    }
    let runs: Vec<_> = [140usize, 80, 40]
        .into_iter()
        .map(|cols| std::thread::spawn(move || (cols, in_terminal(&format!("stty cols {cols}; exec {BIN} --status-demo"), &[]))))
        .collect();
    for run in runs {
        let (cols, output) = run.join().unwrap();
        assert!(output.starts_with("\x1b[?25l"), "{cols}: hides the cursor first: {:?}", &output[..40.min(output.len())]);
        let hidden = output.rfind("\x1b[?25l").unwrap();
        let shown = output.rfind("\x1b[?25h").expect("cursor shown again");
        assert!(shown > hidden, "{cols}: the cursor is shown after it was last hidden");
        let summary = last_shown_line(&output);
        assert!(summary.starts_with("✓ done  ") && summary.contains(" · 50 files · "), "{cols}: {summary:?}");
        assert!(output.ends_with("\r\n"), "{cols}: ends on a fresh line");

        let frames = frames(&output);
        assert!(frames.len() > 60, "{cols}: only {} frames", frames.len());
        for frame in &frames {
            let shown = visible(frame.trim_end_matches('\r'));
            assert!(columns(&shown) < cols, "{cols}: frame {} columns wide: {shown:?}", columns(&shown));
        }
        let all: String = frames.iter().map(|f| visible(f)).collect();
        for phase in ["reading", "unwrapping", "copying", "relinking"] {
            assert!(all.contains(phase), "{cols}: no {phase} frame");
        }
        if cols >= 140 {
            assert!(all.contains("copying app0") && all.contains("MiB/s") && all.contains("ETA"), "{cols}");
            assert!(all.contains("日本語テキスト.bundle"), "{cols}: the wide file name is shown");
        }
        // Log lines are printed whole, above the line.
        assert!(output.contains("\r\x1b[Jpackage     demo.pkg\r\n"), "{cols}");
    }
}

#[test]
fn verbose_errors_in_a_terminal_restore_it() {
    if !has_script() {
        return;
    }
    let dir = scratch("live-errors");
    let short = dir.join("short.bin");
    fs::write(&short, b"hello world, not a package").unwrap();
    let output = in_terminal(&format!("exec {BIN} -v {} {}", short.display(), dir.join("out").display()), &[]);
    assert!(output.starts_with("\x1b[?25l"), "{output:?}");
    let failed = output.find("✗").expect("failure line");
    assert!(output[..failed].ends_with("\x1b[?25h\x1b[1m\x1b[38;2;255;85;95m"), "{output:?}");
    assert!(
        output.ends_with("error: invalid package: read of 0x100 bytes at 0x0 is past the end of the 0x1a-byte package\r\n"),
        "{output:?}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ctrl_c_restores_the_terminal_and_exits_by_the_signal() {
    if !has_script() {
        return;
    }
    if Command::new("timeout").arg("--version").output().is_err() {
        eprintln!("note: coreutils `timeout` not found; skipping");
        return;
    }
    let output = in_terminal(&format!("stty cols 100; timeout --preserve-status -s INT 1.2 {BIN} --status-demo; echo \"EXIT=$?\""), &[]);
    // 130 is how a shell reports death by SIGINT.
    assert!(output.ends_with("\x18\r\x1b[0m\x1b[J\x1b[?25hEXIT=130\r\n"), "{:?}", &output[output.len().saturating_sub(200)..]);
    assert!(!output.contains("✓ done"));
}

#[test]
fn the_relinker_gets_the_terminal_to_itself() {
    if !has_script() {
        return;
    }
    let dir = scratch("relinker");
    let relinker = dir.join("fake-relinker");
    fs::write(
        &relinker,
        "#!/bin/sh\n\
         echo 'relinker out 1'\n\
         echo 'relinker err 1' >&2\n\
         printf '\\033[32mcoloured\\033[0m\\n'\n\
         [ -t 1 ] && echo 'stdout is a terminal'\n\
         [ -t 2 ] && echo 'stderr is a terminal' >&2\n\
         sleep 0.3\n\
         echo 'relinker out 2'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&relinker, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output =
        in_terminal(&format!("stty cols 120; exec {BIN} --status-demo"), &[("PKG_TO_ANYPS5_DEMO_RELINKER", relinker.to_str().unwrap())]);
    // Verbatim, in order, nothing drawn in between, colours untouched.
    let block =
        "relinker out 1\r\nrelinker err 1\r\n\x1b[32mcoloured\x1b[0m\r\nstdout is a terminal\r\nstderr is a terminal\r\nrelinker out 2\r\n";
    let start = output.find(block).unwrap_or_else(|| panic!("relinker output not verbatim: {output:?}"));
    // The line was erased and the cursor shown just before; the log line
    // naming the relinker came right before that, redrawing the line once.
    let before = &output[..start];
    assert!(before.ends_with("\r\x1b[0m\x1b[J\x1b[?25h"), "{:?}", &before[before.len().saturating_sub(300)..]);
    let log = before.rfind(&format!("\r\x1b[Jrelink      {}\r\n", relinker.display())).expect("relink log line");
    assert!(frames(&before[log..]).len() <= 1, "no animation once the relinker was announced");
    // Then the animation comes back: cursor hidden, frames of the relinking phase, the summary.
    let after = &output[start + block.len()..];
    assert!(after.starts_with("\x1b[?25l"), "{:?}", &after[..80.min(after.len())]);
    assert!(after.contains("relinker exited (exit status: 0)"));
    let resumed: Vec<_> = frames(after).into_iter().filter(|f| visible(f).contains("relinking")).collect();
    assert!(resumed.len() >= 3, "only {} frames after the relinker", resumed.len());
    assert!(last_shown_line(after).starts_with("✓ done  "));
    fs::remove_dir_all(&dir).unwrap();
}
