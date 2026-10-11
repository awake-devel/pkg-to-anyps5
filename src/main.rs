//! pkg-to-anyps5: extracts a PS5 debug package into the layout the AnyPS5
//! relinker takes as input and the layout its output runs from.

mod bytes;
mod error;
mod fih;
mod inner;
mod kraken;
mod naps;
mod pfs;
mod selfelf;
mod source;
mod status;

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use error::{Error, Result};
use fih::{Cnt, Fih};
use inner::{InnerFile, InnerImage};
use pfs::OuterPfs;
use source::Source;
use status::{Phase, Status};

const USAGE: &str = "\
usage: pkg-to-anyps5 <package.pkg> <output-dir> [options]
       pkg-to-anyps5 <package.pkg> --list

Writes:
  <output-dir>/source/eboot.bin          main executable, unwrapped to ELF (relinker input)
  <output-dir>/source/sce_module/*.prx   bundled modules, unwrapped to ELF
  <output-dir>/source/<path>.prx         modules shipped elsewhere (e.g. Media/Modules/),
                                         unwrapped at their own path for the relinker
  <output-dir>/app0/                     every other game file, plus sce_sys/ metadata

options:
  --list               print the package's files and sizes, write nothing
  --executables-only   write source/ only (seconds instead of a full copy)
  --jobs <n>           files decoded at once (default 4)
  --only <path>        write only app0/ files at or under this path (repeatable)
  --relink <build-dir> afterwards run <build-dir>/core/relinker/relinker on
                       source/eboot.bin to make <output-dir>/app.elf, and copy
                       <build-dir>/core/libs/libs/*.prx into <output-dir>/libs/
                       DEPRECATED: no longer maintained, removed in v1
  --module-dir <dir>   with --relink: also convert the modules of this package
                       directory that the game loads at run time, such as Unity's
                       Media/Plugins (needs a relinker with --module-dir, AnyPS5 #846)
                       DEPRECATED with --relink, removed in v1
  --windows            with --relink: produce app.exe for Windows instead
                       DEPRECATED with --relink, removed in v1
  -v, --verbose        show a live status line: phase, progress bar, speed,
                       ETA and the current file (plain lines without a terminal)
  -h, --help           print this help
  -V, --version        print the version";

/// The NAPS layout is a few MiB even for a 100 GB package.
const MAX_NAPS_SIZE: u64 = 1 << 30;

/// Executables are unwrapped in memory; anything larger is a corrupt size.
const MAX_EXECUTABLE_SIZE: u64 = 2 << 30;

/// Printed whenever --relink is used, until the option is removed in v1.
const RELINK_DEPRECATED: &str = "warning: --relink (with --module-dir and --windows) is no longer maintained \
and will be removed in v1; run the AnyPS5 relinker on <output-dir>/source/eboot.bin yourself";

enum Parsed {
    Run(Options),
    Help,
    Version,
    StatusDemo,
}

struct Options {
    package: PathBuf,
    output: Option<PathBuf>,
    list: bool,
    executables_only: bool,
    relink: Option<PathBuf>,
    windows: bool,
    only: Vec<String>,
    module_dirs: Vec<String>,
    jobs: usize,
    verbose: bool,
}

fn parse_args() -> std::result::Result<Parsed, String> {
    let mut args = std::env::args_os().skip(1);
    let mut positional = Vec::new();
    let mut options = Options {
        package: PathBuf::new(),
        output: None,
        list: false,
        executables_only: false,
        relink: None,
        windows: false,
        only: Vec::new(),
        module_dirs: Vec::new(),
        jobs: 4,
        verbose: false,
    };
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--list") => options.list = true,
            Some("--executables-only") => options.executables_only = true,
            Some("--windows") => options.windows = true,
            Some("-v" | "--verbose") => options.verbose = true,
            // Not in the help: runs the status line on simulated progress.
            Some("--status-demo") if std::env::args_os().len() == 2 => return Ok(Parsed::StatusDemo),
            Some("--jobs") => {
                let n = args.next().ok_or("--jobs needs a number")?;
                options.jobs = n.to_str().and_then(|n| n.parse().ok()).filter(|&n| n > 0).ok_or("--jobs needs a positive number")?;
            }
            Some("--module-dir") => {
                let dir = args.next().ok_or("--module-dir needs a directory inside the package")?;
                options.module_dirs.push(dir.to_str().ok_or("--module-dir path is not UTF-8")?.trim_matches('/').to_owned());
            }
            Some("--only") => {
                let path = args.next().ok_or("--only needs a path inside the package")?;
                options.only.push(path.to_str().ok_or("--only path is not UTF-8")?.trim_matches('/').to_owned());
            }
            Some("--relink") => options.relink = Some(args.next().ok_or("--relink needs the AnyPS5 build directory")?.into()),
            Some("-h" | "--help") => return Ok(Parsed::Help),
            Some("-V" | "--version") => return Ok(Parsed::Version),
            Some(s) if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    let mut positional = positional.into_iter();
    options.package = positional.next().ok_or("missing the package path")?;
    options.output = positional.next();
    if positional.next().is_some() {
        return Err("too many arguments".into());
    }
    if options.output.is_none() && !options.list {
        return Err("missing the output directory".into());
    }
    if options.windows && options.relink.is_none() {
        return Err("--windows only applies with --relink".into());
    }
    if !options.module_dirs.is_empty() && options.relink.is_none() {
        return Err("--module-dir only applies with --relink".into());
    }
    if options.executables_only && !options.only.is_empty() {
        return Err("--only selects app0/ files; --executables-only writes none".into());
    }
    if options.list && (options.executables_only || options.relink.is_some()) {
        return Err("--list writes nothing; it cannot be combined with --executables-only or --relink".into());
    }
    Ok(Parsed::Run(options))
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("pkg-to-anyps5 {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::StatusDemo) => {
            status::demo();
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::from(1);
        }
    };
    // On stderr, so stdout (and anything parsing it) is unchanged.
    if options.relink.is_some() {
        eprintln!("{RELINK_DEPRECATED}");
    }
    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
    }
}

fn run(options: &Options) -> Result<()> {
    let started = Instant::now();
    let steps = 2 + u8::from(!options.executables_only) + u8::from(options.relink.is_some());
    let status = Status::new(options.verbose, steps, started);
    status.phase(Phase::Reading, 0, 0);
    status.set_file(&options.package.display().to_string());
    let src = Source::open(&options.package)?;
    let fih = Fih::read(&src)?;
    let cnt = Cnt::read(&src, fih.cnt_offset)?;
    status.line(format!("package     {}", options.package.display()));
    status.line(format!("content ID  {}", cnt.content_id));
    status.line(format!("FIH         format {}, PFS {:#x}+{:#x}", fih.format_version, fih.pfs_offset, fih.pfs_size));

    let outer = OuterPfs::open(&src, fih.pfs_offset, fih.pfs_size, fih.superblock_offset)?;
    status.line(format!(
        "outer PFS   {} inodes, block size {:#x}, seed {}",
        outer.inode_count(),
        outer.superblock().block_size,
        String::from_utf8_lossy(&outer.superblock().seed)
    ));
    let image_ino = outer.find("pfs_image.dat")?.ok_or_else(|| Error::format("the outer PFS has no pfs_image.dat"))?;
    let naps_ino = outer
        .find("naps_pkg_layout.dat")?
        .ok_or_else(|| Error::unsupported("the outer PFS has no naps_pkg_layout.dat; images without a NAPS layout are not supported"))?;
    let naps_file = outer.file(naps_ino)?;
    if naps_file.size > MAX_NAPS_SIZE {
        return Err(Error::format(format!("naps_pkg_layout.dat is {} bytes, far larger than any layout", naps_file.size)));
    }
    let naps_blob = naps_file.read_all()?;
    let image = InnerImage::open(outer.file(image_ino)?, &naps_blob)?;
    status.line(format!(
        "inner image {:.2} GiB logical in {} blocks ({} stored, {} Kraken, {} sparse)",
        gib(image.mount),
        image.block_count(),
        image.stats.stored,
        image.stats.kraken,
        image.stats.sparse
    ));
    let files = image.files()?;
    let total: u64 = files.iter().map(|f| f.size).sum();
    status.line(format!("files       {} under uroot, {:.2} GiB", files.len(), gib(total)));

    if options.list {
        status.close();
        for file in &files {
            println!("{:>16}  {}", file.size, file.path);
        }
        for name in &cnt.skipped_encrypted {
            println!("{:>16}  sce_sys/{name} (encrypted CNT entry, not extractable)", "-");
        }
        return Ok(());
    }

    let out = options.output.as_deref().expect("checked in parse_args");
    let source_dir = out.join("source");
    let app0 = out.join("app0");
    create_dir(&source_dir)?;

    let executables: Vec<&InnerFile> = files.iter().filter(|f| is_relinker_input(&f.path) || is_game_module(&f.path)).collect();
    let mut resources: Vec<&InnerFile> = files.iter().filter(|f| !is_relinker_input(&f.path)).collect();
    if !options.only.is_empty() {
        resources.retain(|f| options.only.iter().any(|p| f.path == *p || f.path.starts_with(&format!("{p}/"))));
        if resources.is_empty() {
            return Err(Error::format(format!("no app0/ file matches --only {}", options.only.join(", "))));
        }
    }
    if !executables.iter().any(|f| f.path == "eboot.bin") {
        return Err(Error::format("the package has no eboot.bin"));
    }
    status.phase(Phase::Unwrapping, executables.iter().map(|f| f.size).sum(), executables.len() as u64);
    // source/ mirrors the game tree: eboot.bin and sce_module/ beside it, and
    // every other game module unwrapped at its own path. The relinker looks
    // up needed modules anywhere under source/ and writes each converted
    // module to the same path under app0/ with a .guest.prx suffix, which is
    // where AnyPS5's module loader looks for it.
    for file in &executables {
        let required = is_relinker_input(&file.path);
        if file.size > MAX_EXECUTABLE_SIZE {
            return Err(Error::format(format!("{} is {} bytes, too large for an executable", file.path, file.size)));
        }
        status.set_file(&file.path);
        let mut bytes = vec![0u8; file.size as usize];
        image.read_at(file.logical, &mut bytes)?;
        status.advance(file.size);
        status.file_done();
        let unwrapped = if selfelf::is_self(&bytes) {
            selfelf::unwrap(&bytes).map(Some)
        } else if selfelf::is_elf(&bytes) {
            Ok(Some(bytes))
        } else {
            Ok(None)
        };
        let elf = match unwrapped {
            Ok(Some(elf)) => elf,
            Ok(None) if required => return Err(Error::format(format!("{} is neither a SELF nor an ELF", file.path))),
            Ok(None) => continue,
            Err(err) if required => return Err(Error::format(format!("{}: {err}", file.path))),
            Err(err) => {
                status.line(format!("  skipped {}: {err}", file.path));
                continue;
            }
        };
        write_file(&join_inside(&source_dir, &file.path)?, &elf)?;
        status.wrote(&file.path, file.size);
        status.line(format!("  source/{:<40} ELF type {:#06x}, {} bytes", file.path, selfelf::elf_type(&elf).unwrap_or(0), elf.len()));
    }

    if !options.executables_only {
        create_dir(&app0.join("sce_sys"))?;
        for cnt_file in &cnt.files {
            write_file(&join_inside(&app0.join("sce_sys"), &cnt_file.name)?, &cnt_file.bytes)?;
        }
        status.line(format!(
            "  app0/sce_sys/  {} metadata files from the CNT ({} encrypted entries skipped)",
            cnt.files.len(),
            cnt.skipped_encrypted.len()
        ));
        status.phase(Phase::Copying, resources.iter().map(|f| f.size).sum(), resources.len() as u64);
        let written = copy_resources(&image, &resources, &app0, options.jobs, started, &status)?;
        // Counted after the copy: a package file at the same path replaced
        // the CNT copy on disk, and the summary counts each path once.
        for cnt_file in &cnt.files {
            status.wrote(&format!("sce_sys/{}", cnt_file.name), cnt_file.bytes.len() as u64);
        }
        status.line(format!("  app0/  {} files, {:.2} GiB", resources.len(), gib(written)));
    }

    if let Some(build) = &options.relink {
        status.phase(Phase::Relinking, 0, 0);
        relink(build, out, options.windows, &options.module_dirs, &status)?;
    }
    status.finish();
    Ok(())
}

/// Copies files on `jobs` threads, largest first so the long files start
/// early. The first error stops the other threads and is returned.
fn copy_resources(image: &InnerImage, files: &[&InnerFile], app0: &Path, jobs: usize, started: Instant, status: &Status) -> Result<u64> {
    let mut order: Vec<&InnerFile> = files.to_vec();
    order.sort_by_key(|f| std::cmp::Reverse(f.size));
    let total: u64 = order.iter().map(|f| f.size).sum();
    let next = AtomicUsize::new(0);
    let written = AtomicU64::new(0);
    let next_report = AtomicU64::new(0);
    let failed = AtomicBool::new(false);
    let first_error = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| {
                while !failed.load(Ordering::Relaxed) {
                    let Some(file) = order.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                    status.set_file(&file.path);
                    let result = copy_one(image, file, app0, &failed, |n| {
                        let done = written.fetch_add(n, Ordering::Relaxed) + n;
                        status.advance(n);
                        // The status line shows progress continuously.
                        if status.is_verbose() {
                            return;
                        }
                        let due = next_report.load(Ordering::Relaxed);
                        if done >= due && next_report.compare_exchange(due, done + (4 << 30), Ordering::Relaxed, Ordering::Relaxed).is_ok()
                        {
                            let rate = gib(done) / started.elapsed().as_secs_f64().max(0.001);
                            println!("  app0/  {:.2} / {:.2} GiB ({:.2} GiB/s)", gib(done), gib(total), rate);
                        }
                    });
                    match result {
                        Ok(()) => {
                            status.file_done();
                            status.wrote(&file.path, file.size);
                        }
                        Err(err) => {
                            failed.store(true, Ordering::Relaxed);
                            first_error.lock().unwrap().get_or_insert(err);
                        }
                    }
                }
            });
        }
    });
    match first_error.into_inner().unwrap() {
        Some(err) => Err(err),
        None => Ok(written.into_inner()),
    }
}

fn copy_one(image: &InnerImage, file: &InnerFile, app0: &Path, failed: &AtomicBool, mut progress: impl FnMut(u64)) -> Result<()> {
    let target = join_inside(app0, &file.path)?;
    if let Some(parent) = target.parent() {
        create_dir(parent)?;
    }
    let handle = fs::File::create(&target).map_err(Error::io(format!("create {}", target.display())))?;
    let mut writer = BufWriter::with_capacity(4 << 20, handle);
    let result = image
        .copy_to(file.logical, file.size, |chunk| {
            if failed.load(Ordering::Relaxed) {
                return Err(Error::format("stopped after an error in another file"));
            }
            writer.write_all(chunk).map_err(Error::io(format!("write {}", target.display())))?;
            progress(chunk.len() as u64);
            Ok(())
        })
        .and_then(|()| writer.flush().map_err(Error::io(format!("write {}", target.display()))));
    if result.is_err() {
        // Never leave a partial file behind.
        drop(writer);
        let _ = fs::remove_file(&target);
    }
    result
}

/// Joins a package path to an output directory, refusing any path that
/// could reach outside it.
fn join_inside(base: &Path, relative: &str) -> Result<PathBuf> {
    if !fih::is_plain_relative_path(relative) || (cfg!(windows) && !is_windows_safe_path(relative)) {
        return Err(Error::format(format!("refusing to write the unsafe package path {relative:?}")));
    }
    Ok(base.join(relative))
}

/// Windows opens device names such as `CON` or `nul.txt` instead of a file,
/// strips trailing dots and spaces, and forbids a few characters.
fn is_windows_safe_path(relative: &str) -> bool {
    const DEVICES: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4",
        "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    relative.split('/').all(|part| {
        let stem = part.split('.').next().unwrap_or(part).trim_end().to_ascii_lowercase();
        !DEVICES.contains(&stem.as_str())
            && !part.ends_with(['.', ' '])
            && !part.contains(['<', '>', '"', '|', '?', '*'])
            && !part.chars().any(|c| c.is_control())
    })
}

/// Files the relinker reads from beside its input: the executable and the
/// modules in sce_module/ (other names it accepts are sce_modules/ and prx/).
fn is_relinker_input(path: &str) -> bool {
    if path == "eboot.bin" {
        return true;
    }
    match path.split_once('/') {
        Some(("sce_module" | "sce_modules" | "prx", rest)) => !rest.contains('/'),
        _ => false,
    }
}

/// A module the game ships outside sce_module/, such as Unity's
/// Media/Modules/*.prx. fakelib/ holds stubs of system libraries, which
/// AnyPS5 provides itself, and sce_sys/ holds system metadata.
fn is_game_module(path: &str) -> bool {
    let module = path.ends_with(".prx") || path.ends_with(".sprx");
    module && !path.starts_with("fakelib/") && !path.starts_with("sce_sys/")
}

fn relink(build: &Path, out: &Path, windows: bool, module_dirs: &[String], status: &Status) -> Result<()> {
    let relinker = build.join(format!("core/relinker/relinker{}", std::env::consts::EXE_SUFFIX));
    let libs = build.join("core/libs/libs");
    if !relinker.is_file() {
        return Err(Error::format(format!("no relinker at {}", relinker.display())));
    }
    let output = out.join(if windows { "app.exe" } else { "app.elf" });
    let mut command = Command::new(&relinker);
    if windows {
        command.arg("--windows");
    }
    for dir in module_dirs {
        command.arg("--module-dir").arg(dir);
    }
    command.arg(out.join("source/eboot.bin")).arg(&output);
    status.set_file(&format!("source/eboot.bin → {}", output.file_name().unwrap_or_default().to_string_lossy()));
    status.line(format!("relink      {}", relinker.display()));
    let exit = status.run(&mut command).map_err(Error::io(format!("run {}", relinker.display())))?;
    if !exit.success() {
        return Err(Error::format(format!("the relinker failed ({exit})")));
    }
    #[cfg(unix)]
    if !windows {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&output, fs::Permissions::from_mode(0o755)).map_err(Error::io(format!("chmod {}", output.display())))?;
    }
    let lib_dir = out.join("libs");
    create_dir(&lib_dir)?;
    let mut copied = 0;
    for entry in fs::read_dir(&libs).map_err(Error::io(format!("list {}", libs.display())))? {
        let path = entry.map_err(Error::io(format!("list {}", libs.display())))?.path();
        if path.extension().is_some_and(|e| e == "prx") {
            fs::copy(&path, lib_dir.join(path.file_name().unwrap())).map_err(Error::io(format!("copy {}", path.display())))?;
            copied += 1;
        }
    }
    status.line(format!("  {} and libs/ ({copied} system libraries) are ready", output.display()));
    Ok(())
}

fn create_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(Error::io(format!("create {}", path.display())))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    fs::write(path, bytes).map_err(|err| {
        // Never leave a partial file behind.
        let _ = fs::remove_file(path);
        Error::io(format!("write {}", path.display()))(err)
    })
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_executable_and_top_level_modules_go_to_source() {
        assert!(is_relinker_input("eboot.bin"));
        assert!(is_relinker_input("sce_module/libc.prx"));
        assert!(!is_relinker_input("sce_module/sub/x.prx"));
        assert!(!is_relinker_input("fakelib/libSceAgc.prx"));
        assert!(!is_relinker_input("data/eboot.bin"));
    }

    #[test]
    fn package_paths_cannot_leave_the_output_directory() {
        let base = Path::new("/out/app0");
        assert_eq!(join_inside(base, "Media/level0").unwrap(), base.join("Media/level0"));
        for bad in ["../x", "/etc/passwd", "a/../../x", ""] {
            assert!(join_inside(base, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn windows_device_names_and_trailing_dots_are_unsafe() {
        assert!(is_windows_safe_path("Media/level0.assets"));
        assert!(is_windows_safe_path("console/config.txt"));
        for bad in ["CON", "data/nul.txt", "Aux", "com1.bin", "x.", "x ", "a?b"] {
            assert!(!is_windows_safe_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn modules_elsewhere_in_the_tree_are_unwrapped_but_stubs_are_not() {
        assert!(is_game_module("Media/Modules/Il2cppUserAssemblies.prx"));
        assert!(is_game_module("localcacheps5/fullgame.prx"));
        assert!(!is_game_module("fakelib/libSceAgc.sprx"));
        assert!(!is_game_module("sce_sys/about/right.sprx"));
        assert!(!is_game_module("Media/level0"));
    }
}
