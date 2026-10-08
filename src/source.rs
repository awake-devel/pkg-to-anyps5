//! Positional reads from the package file.

use std::fs::File;
use std::os::unix::fs::FileExt;
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
        self.file.read_exact_at(buf, offset).map_err(Error::io(format!("read {:#x} bytes at {offset:#x}", buf.len())))
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
