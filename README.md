# pkg-to-anyps5

Extracts a PS5 debug package (`.pkg` with the `\x7FFIH` header) into the
layout the [AnyPS5](https://github.com/boykopovar/AnyPS5) relinker takes as
input and the layout its output runs from.

AnyPS5 converts a plain ELF `eboot.bin` plus the game's modules. A package
stores them as fake-signed SELF files inside a compressed filesystem image, so
this tool unpacks the image, unwraps every SELF into an ELF, and lays the
files out for the relinker.

## Install

Download a ready-made binary for Windows, Linux (x86_64, aarch64) or macOS
(Apple silicon) from the
[releases page](https://github.com/awake-devel/pkg-to-anyps5/releases), unpack
it and run `pkg-to-anyps5` (`pkg-to-anyps5.exe` on Windows) from a terminal.

Or build it with Rust 1.88 or newer:

```sh
cargo install --git https://github.com/awake-devel/pkg-to-anyps5
# or, from a clone
cargo build --release   # binary at target/release/pkg-to-anyps5
```

No dependencies beyond the Rust standard library. The
[wiki](https://github.com/awake-devel/pkg-to-anyps5/wiki) has a step-by-step
guide.

## Use

```sh
# What is in the package
pkg-to-anyps5 game.pkg --list

# Executables only (seconds), then relink with an AnyPS5 build
pkg-to-anyps5 game.pkg out --executables-only --relink ~/AnyPS5/build

# Everything: executables, every game file, relinked and ready to run
pkg-to-anyps5 game.pkg out --relink ~/AnyPS5/build
./out/app.elf
```

| Option | Effect |
|---|---|
| `--list` | Print the package's files and sizes; write nothing. |
| `--executables-only` | Write `source/` only (seconds instead of a full copy). |
| `--jobs <n>` | Files decoded at once (default 4). |
| `--only <path>` | Write only the `app0/` files at or under a path (repeatable), to check a few files without copying a whole game. |
| `--relink <build-dir>` | Afterwards run `<build-dir>/core/relinker/relinker` (`relinker.exe` on Windows) on `source/eboot.bin` to make `app.elf`, and copy AnyPS5's system libraries into `libs/`. |
| `--module-dir <dir>` | With `--relink`: also convert the modules of this package directory that the game loads at run time, such as Unity's `Media/Plugins` (needs a relinker with `--module-dir`). Repeatable. |
| `--windows` | With `--relink`: produce `app.exe` for Windows instead. |
| `-h`, `--help` / `-V`, `--version` | Print help or the version. |

Exit status is 0 on success, 1 for a usage error and 2 when the package
cannot be read.

Output:

```text
out/
  source/                 relinker input, mirrors the game tree
    eboot.bin             main executable, unwrapped to ELF
    sce_module/*.prx      bundled modules, unwrapped to ELF
    <path>/*.prx          modules shipped elsewhere (e.g. Unity's Media/Modules/)
  app0/                   every game file at its package path
    sce_sys/              param.json, icons, PlayGo tables from the CNT container
  app.elf, libs/          with --relink: the converted game and AnyPS5's system libraries
  app.exe                 with --relink --windows, instead of app.elf
```

`source/` mirrors the game tree because the relinker looks for a needed module
anywhere beside its input and writes the converted module to the same relative
path under `app0/` with a `.guest.prx` suffix. That is also where AnyPS5's
module loader looks when the game opens `/app0/<path>.prx`.

## What it reads

| Layer | Format |
|---|---|
| `\x7FFIH` header | little-endian; PFS segment at 0x10000, superblock and CNT offsets. Signed byte 0x80 (retail) is refused. |
| `\x7FCNT` container | big-endian entry table; unencrypted entries go to `app0/sce_sys/`, encrypted ones (licenses) are skipped. |
| Outer PFS | signed inodes (0x2C8 bytes, a 32-byte hash before every block pointer), direct and indirect blocks. Block `b` is at `pfs_offset + b × block_size`, both read from the headers (0x10000 in every package seen so far). Only the plain `PPRPLAIN-NOAUTH!` seed is readable. |
| `naps_pkg_layout.dat` | header, file-offset table, u2c table, 9-byte CblockInfo records mapping each 256 KiB logical block to stored bytes. |
| `pfs_image.dat` | 256 KiB blocks, stored or Kraken-compressed without Oodle headers; the inner superblock and flat inodes (0xA8 bytes) follow the last file. |
| SELF | 32-byte header, segment table, ELF header; blocked segments are copied to their program header offsets. Encrypted or compressed segments are refused. |

The CblockInfo table does not always start at the same alignment: some
packages pad the file-offset table to 16 bytes, others start the u2c table
right after it. Both placements are tried, and only the one whose blocks cover
the image exactly is used.

Every inconsistency is an error. The tool never writes a partial or guessed
file: a file being written when an error occurs is removed. Files finished
before the error stay, so delete the output directory after a failed run. Package paths that
could leave the output directory (`..`, absolute paths) are refused, and sizes
read from headers are checked before anything is allocated.

## Tested

| Package | Result |
|---|---|
| Large package: 93 GB, 172 files, 134 GiB unpacked, 450,700 Kraken blocks | Tree listed, executables relinked; sample files decoded with correct signatures (Bink videos, SELF, a 2 GB data archive). Full copy not run. |
| Unity package: 2.4 GB, 2,144 files, 7.7 GiB unpacked, one sparse block | Full extraction and relink in 24 s; seven guest modules built. |

How far a game runs is up to AnyPS5. Both packages stop at load time on a
system function it does not implement yet.

## Limits

- Kraken entropy types 1 (tANS), 3 (RLE) and 5 (recursive) are not
  implemented; a block using them is reported, not guessed.
- Inner images in a layout other than the flat mode 0x10 are refused.
- Each file is decoded on one thread, at about 55 MB/s; `--jobs` decodes
  several files at once.
- Modules a game loads at runtime without importing them (Unity's
  `Media/Plugins/*.prx`) are unwrapped into `source/`, but the relinker only
  converts them when asked with `--module-dir`.

## Legal

This project is not affiliated with, endorsed by or sponsored by Sony
Interactive Entertainment. PlayStation and PS5 are trademarks of Sony
Interactive Entertainment Inc., named here only to describe the format the
tool reads.

The tool contains no Sony code, keys or decryption. It reads only packages
whose contents are already unencrypted: retail packages, encrypted
filesystems and encrypted SELF segments are refused, never bypassed. It
exists for interoperability, so that independently written software
(AnyPS5) can load programs that are packaged for another system.

Use it only with packages you built yourself or are otherwise entitled to
use. You are responsible for complying with the laws and licence agreements
that apply to you. Do not open issues that ask for or link to copyrighted
game content.

## Credits and license

The NAPS and inner-image formats and the Kraken decoder follow
[PS5PCEM](https://github.com/iStark/PS5PCEM) (`src/pkg/`), which in turn
follows [LibProsperoPKG](https://github.com/SvenGDK/LibProsperoPKG). Both are
GPL-3.0, and the Kraken decoder here is a port, so this tool is licensed
GPL-3.0-or-later (see `LICENSE`). AnyPS5 itself is GPL-2.0-only; the two are
separate programs and this tool only runs the relinker as a process.
