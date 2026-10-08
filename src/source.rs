//! Positional reads from the package file.

use std::fs::File;
use std::path::Path;

use crate::error::{Error, Result};

pub struct Source {
    file: File,
    len: u64,
}

impl Source {
    pub fn open(path: &Path) -> Result<Source> {
        let file = File::open(path).map_err(Error::io(format!("open {}", path.display())))?;
        let len = file.metadata().map_err(Error::io(format!("stat {}", path.display())))?.len();
        Ok(Source { file, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        let end = offset.checked_add(buf.len() as u64);
        if end.is_none_or(|end| end > self.len) {
            return Err(Error::format(format!(
                "read of {:#x} bytes at {offset:#x} is past the end of the {:#x}-byte package",
                buf.len(),
                self.len
            )));
        }
        read_exact_at(&self.file, buf, offset).map_err(Error::io(format!("read {:#x} bytes at {offset:#x}", buf.len())))
    }

    /// Reads `len` bytes at `offset`, checking the range before allocating.
    pub fn read_vec(&self, len: u64, offset: u64) -> Result<Vec<u8>> {
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(Error::format(format!(
                "read of {len:#x} bytes at {offset:#x} is past the end of the {:#x}-byte package",
                self.len
            )));
        }
        let mut buf = vec![0u8; len as usize];
        self.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }
}

/// A positional read that leaves the file cursor alone, so threads can share
/// one handle.
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// `seek_read` may return fewer bytes than asked, so loop until the buffer is
/// full. It moves the cursor, but every read here names its own offset.
#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}
