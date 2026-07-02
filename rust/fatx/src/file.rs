use std::cmp::min;
use std::io;
use std::io::Write as _;
use std::time::SystemTime;

use crate::datetime::DateTime;
use crate::dir::DirectoryEntry;
use crate::error::Error;
use crate::fat::{ClusterId, FatEntry};
use crate::fs::{FatxFs, FatxFsHandle};

pub struct File {
    handle: FatxFsHandle,
    dirent: DirectoryEntry,
    cur_cluster_relative: u32,
    cur_cluster_absolute: ClusterId,
    seek_pos: u32,
}

impl File {
    pub(crate) fn new(handle: FatxFsHandle, dirent: DirectoryEntry) -> Self {
        Self {
            handle,
            seek_pos: 0,
            cur_cluster_relative: 0,
            cur_cluster_absolute: dirent.first_cluster(),
            dirent,
        }
    }

    pub fn file_size(&self) -> u32 {
        self.dirent.file_size()
    }

    /// Advances the cached cluster-chain cursor to the cluster containing
    /// byte `seek_pos`, restarting the walk from the first cluster if
    /// `seek_pos` moved backwards. If `alloc` is true, allocates and
    /// attaches new (zero-filled) clusters on demand when the walk runs
    /// past the end of the existing chain -- used by `write`, and by
    /// `set_len` when growing. Shared by both the read and write paths so
    /// the cluster-walk logic isn't duplicated.
    fn locate_cluster(&mut self, fs: &mut FatxFs, alloc: bool) -> Result<ClusterId, Error> {
        let target_cluster_relative = (self.seek_pos as u64 / fs.num_bytes_per_cluster) as u32;

        if target_cluster_relative < self.cur_cluster_relative {
            self.cur_cluster_relative = 0;
            self.cur_cluster_absolute = self.dirent.first_cluster();
        }

        for _ in self.cur_cluster_relative..target_cluster_relative {
            self.cur_cluster_absolute = match fs.fat.entry(self.cur_cluster_absolute)? {
                FatEntry::Data(cluster) => cluster,
                FatEntry::End if alloc => {
                    let new_cluster = fs.alloc_cluster(true)?;
                    fs.attach_cluster(self.cur_cluster_absolute, new_cluster)?;
                    new_cluster
                }
                _ => return Err(Error::InvalidClusterChain),
            };
        }
        self.cur_cluster_relative = target_cluster_relative;

        Ok(self.cur_cluster_absolute)
    }

    /// Grows or shrinks the file to exactly `new_size` bytes. Shrinking
    /// frees every cluster past the one containing the new last byte, but
    /// -- like `libfatx` -- does not zero the shrunk-away tail of the
    /// retained cluster; growing therefore can't just rely on newly
    /// allocated clusters already being zero-filled (that only covers
    /// clusters allocated fresh, not growth back into a cluster that was
    /// previously shrunk within). So growing delegates to `write`'s
    /// zero-fill loop for the whole `[old_size, new_size)` range, which
    /// covers both cases uniformly. Matches the convention (shared with
    /// `libfatx`) that a file's first cluster is retained for its whole
    /// lifetime once created -- `set_len(0)` does not free it, only
    /// `unlink` does.
    pub fn set_len(&mut self, new_size: u32) -> Result<(), Error> {
        let old_size = self.dirent.file_size();

        if new_size > old_size {
            let saved_seek_pos = self.seek_pos;
            self.seek_pos = old_size;
            let zeros = vec![0u8; (new_size - old_size) as usize];
            self.write_all(&zeros)?;
            self.seek_pos = saved_seek_pos;
            return Ok(());
        }

        if new_size < old_size {
            let handle = self.handle.fs.clone();
            let mut fs = handle.lock().unwrap();

            let retained_relative = if new_size == 0 {
                0
            } else {
                (new_size - 1) as u64 / fs.num_bytes_per_cluster
            };
            let mut cluster = self.dirent.first_cluster();
            for _ in 0..retained_relative {
                cluster = match fs.fat.entry(cluster)? {
                    FatEntry::Data(next) => next,
                    _ => return Err(Error::InvalidClusterChain),
                };
            }
            if let Some(next) = fs.fat.next_in_chain(cluster)? {
                fs.free_cluster_chain(next)?;
            }
            fs.write_fat_entry(cluster, FatEntry::End)?;
            self.cur_cluster_relative = 0;
            self.cur_cluster_absolute = self.dirent.first_cluster();

            self.dirent.set_file_size(new_size);
            self.dirent
                .set_modified(DateTime::from_system_time(SystemTime::now()));
            self.dirent.write_back(&mut fs)?;
        }

        Ok(())
    }
}

impl io::Seek for File {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        match pos {
            io::SeekFrom::Start(offset) => {
                self.seek_pos = offset as u32;
            }
            io::SeekFrom::Current(offset) => {
                let target = (self.seek_pos as i64).saturating_add(offset);
                if target < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Seek target cannot be negative",
                    ));
                }
                self.seek_pos = target as u32;
            }
            io::SeekFrom::End(offset) => {
                let target = self.dirent.file_size() as i64 + offset;
                if target < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Seek target cannot be negative",
                    ));
                }
                self.seek_pos = target as u32;
            }
        }
        Ok(self.seek_pos as u64)
    }
}

impl io::Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Get handle to filesystem interface
        let handle = self.handle.fs.clone();
        let mut fs = handle.lock().unwrap();

        let file_size: u32 = self.dirent.file_size();
        let mut buf_pos: u64 = 0;

        while buf_pos < buf.len() as u64 {
            // Ensure we don't read past EOF
            let bytes_remaining_in_file: u32 = file_size - self.seek_pos;
            if bytes_remaining_in_file == 0 {
                break;
            }

            let cluster = self.locate_cluster(&mut fs, false)?;

            // Determine number of bytes to read in this cluster
            let byte_offset_in_cluster = self.seek_pos as u64 % fs.num_bytes_per_cluster;
            let bytes_remaining_in_cluster = fs.num_bytes_per_cluster - byte_offset_in_cluster;
            let bytes_remaining_in_dest = buf.len() as u64 - buf_pos;
            let bytes_to_read_from_cluster = min(
                min(bytes_remaining_in_dest, bytes_remaining_in_cluster),
                bytes_remaining_in_file as u64,
            );

            // Read cluster data from device
            log::debug!(
                "Reading {} bytes from cluster {}",
                bytes_to_read_from_cluster,
                cluster
            );
            fs.seek_cluster(cluster, byte_offset_in_cluster)?;
            let bytes_read = {
                let start = buf_pos as usize;
                let end = start + bytes_to_read_from_cluster as usize;
                fs.device_handle.read(&mut buf[start..end])?
            };

            // Advance
            buf_pos += bytes_read as u64;
            self.seek_pos += bytes_read as u32;
        }

        Ok(buf_pos as usize)
    }
}

impl io::Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let handle = self.handle.fs.clone();
        let mut fs = handle.lock().unwrap();

        // A write starting past the current end of file (a "hole") is
        // handled by the same on-demand allocation as an ordinary append:
        // `locate_cluster(alloc: true)` zero-fills every newly allocated
        // cluster it walks through on the way to the target, which is
        // exactly the hole's zero content -- no separate pre-extension
        // pass is needed.
        let mut buf_pos: u64 = 0;
        while buf_pos < buf.len() as u64 {
            let cluster = self.locate_cluster(&mut fs, true)?;

            let byte_offset_in_cluster = self.seek_pos as u64 % fs.num_bytes_per_cluster;
            let bytes_remaining_in_cluster = fs.num_bytes_per_cluster - byte_offset_in_cluster;
            let bytes_remaining_in_src = buf.len() as u64 - buf_pos;
            let bytes_to_write = min(bytes_remaining_in_src, bytes_remaining_in_cluster);

            log::debug!("Writing {} bytes to cluster {}", bytes_to_write, cluster);
            fs.seek_cluster(cluster, byte_offset_in_cluster)?;
            let start = buf_pos as usize;
            let end = start + bytes_to_write as usize;
            fs.device_handle.write_all(&buf[start..end])?;

            buf_pos += bytes_to_write;
            self.seek_pos += bytes_to_write as u32;
        }

        // Update size (if grown) and mtime once, after the loop -- not
        // per-cluster. Unlike libfatx's `fatx_write`, this also touches
        // `modified` on every successful write (libfatx never does),
        // matching normal POSIX/FUSE expectations.
        if self.seek_pos > self.dirent.file_size() {
            self.dirent.set_file_size(self.seek_pos);
        }
        self.dirent
            .set_modified(DateTime::from_system_time(SystemTime::now()));
        self.dirent.write_back(&mut fs)?;

        Ok(buf_pos as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Write-through design: nothing is buffered in-process.
        Ok(())
    }
}
