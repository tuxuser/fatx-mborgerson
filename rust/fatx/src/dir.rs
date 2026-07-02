use std::path::{Component, Path};

use zerocopy::byteorder::{big_endian, little_endian};
use zerocopy::*;

const FATX_MAX_FILENAME_LEN: usize = 42;

// Markers used in the filename_size field of the directory entry.
const FATX_DELETED_FILE_MARKER: u8 = 0xe5;
const FATX_END_OF_DIR_MARKER: u8 = 0xff;
const FATX_END_OF_DIR_MARKER2: u8 = 0x00;

// Mask to be applied when reading directory entry attributes.
const FATX_ATTR_READ_ONLY: u8 = 1 << 0;
const FATX_ATTR_SYSTEM: u8 = 1 << 1;
const FATX_ATTR_HIDDEN: u8 = 1 << 2;
const FATX_ATTR_VOLUME: u8 = 1 << 3;
const FATX_ATTR_DIRECTORY: u8 = 1 << 4;

// Directory entries are 64 bytes on disk on both platforms. DirectoryEntry
// itself can't be used for this since it holds decoded native fields (plus
// an epoch marker), not a 1:1 image of the wire format.
pub(crate) const FATX_DIRENT_SIZE_BYTES: u64 = 64;

use crate::datetime::DateTime;
use crate::error::Error;
use crate::fat::{ClusterId, FatEntry};
use crate::fs::{FatxFs, FatxFsHandle, Variant};
use crate::path::normalize_virtual_path;

// The directory entry, as it appears on disk on the original Xbox:
// little-endian, Modified/Created/Accessed order, time-then-date pairs.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C, packed)]
struct RawDirentXbox {
    filename_len: u8,
    attributes: u8,
    filename_bytes: [u8; FATX_MAX_FILENAME_LEN],
    first_cluster: little_endian::U32,
    file_size: little_endian::U32,
    modified_time: little_endian::U16,
    modified_date: little_endian::U16,
    created_time: little_endian::U16,
    created_date: little_endian::U16,
    accessed_time: little_endian::U16,
    accessed_date: little_endian::U16,
}

// The directory entry, as it appears on disk on the Xbox 360: big-endian,
// Created/Modified/Accessed order, date-then-time pairs. filename/
// attributes/first_cluster/file_size are laid out identically to the
// original Xbox; only the timestamp block differs. Confirmed against
// free60.org/System-Software/Systems/FATX and a real retail-partitioned
// drive.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C, packed)]
struct RawDirentXbox360 {
    filename_len: u8,
    attributes: u8,
    filename_bytes: [u8; FATX_MAX_FILENAME_LEN],
    first_cluster: big_endian::U32,
    file_size: big_endian::U32,
    created_date: big_endian::U16,
    created_time: big_endian::U16,
    modified_date: big_endian::U16,
    modified_time: big_endian::U16,
    accessed_date: big_endian::U16,
    accessed_time: big_endian::U16,
}

impl RawDirentXbox {
    fn decode(&self, epoch: u16) -> DirectoryEntry {
        DirectoryEntry {
            filename_len: self.filename_len,
            attributes: self.attributes,
            filename_bytes: self.filename_bytes,
            first_cluster: self.first_cluster.into(),
            file_size: self.file_size.into(),
            modified_time: self.modified_time.into(),
            modified_date: self.modified_date.into(),
            created_time: self.created_time.into(),
            created_date: self.created_date.into(),
            accessed_time: self.accessed_time.into(),
            accessed_date: self.accessed_date.into(),
            epoch,
        }
    }
}

impl RawDirentXbox360 {
    fn decode(&self, epoch: u16) -> DirectoryEntry {
        DirectoryEntry {
            filename_len: self.filename_len,
            attributes: self.attributes,
            filename_bytes: self.filename_bytes,
            first_cluster: self.first_cluster.into(),
            file_size: self.file_size.into(),
            modified_time: self.modified_time.into(),
            modified_date: self.modified_date.into(),
            created_time: self.created_time.into(),
            created_date: self.created_date.into(),
            accessed_time: self.accessed_time.into(),
            accessed_date: self.accessed_date.into(),
            epoch,
        }
    }
}

/// A directory entry, decoded into native fields. This is platform-agnostic
/// -- the on-disk byte order, timestamp field order, and timestamp epoch
/// differences between the original Xbox and Xbox 360 are resolved when the
/// entry is read (see `RawDirentXbox`/`RawDirentXbox360`).
#[derive(Debug)]
pub struct DirectoryEntry {
    filename_len: u8,
    attributes: u8,
    filename_bytes: [u8; FATX_MAX_FILENAME_LEN],
    first_cluster: u32,
    file_size: u32,
    modified_time: u16,
    modified_date: u16,
    created_time: u16,
    created_date: u16,
    accessed_time: u16,
    accessed_date: u16,
    epoch: u16,
}

#[derive(Debug, PartialEq)]
pub enum DirectoryEntryKind {
    Valid,
    Deleted,
    EndOfDirectory,
}

impl DirectoryEntry {
    pub(crate) fn from_path<P: AsRef<Path>>(fs: &mut FatxFs, path: P) -> Result<Self, Error> {
        let path = normalize_virtual_path(path);
        let num_components = path.components().count();

        if num_components == 0 {
            return Err(Error::NotADirectory);
        }

        // Step through path components to find target directory
        let mut cwd = None;
        for (comp_idx, comp) in path.components().enumerate() {
            cwd = match comp {
                Component::RootDir => Some(fs.root_cluster),
                Component::Normal(name) => {
                    if cwd.is_none() {
                        return Err(Error::NotFound);
                    }

                    // Scan directory entries to resolve this path component
                    let mut dir_iter = DirectoryEntryIterator::new(cwd.unwrap());
                    loop {
                        // Fetch next directory entry
                        let dirent_result = dir_iter.next(fs);
                        if dirent_result.is_none() {
                            return Err(Error::NotFound);
                        }

                        // Check current directory entry for target match
                        let dirent = dirent_result.unwrap()?;
                        if let DirectoryEntryKind::Valid = dirent.kind() {
                            if dirent.file_name() != name.to_str().unwrap() {
                                continue;
                            }
                            if comp_idx == (num_components - 1) {
                                return Ok(dirent);
                            }
                            if dirent.is_directory() {
                                break Some(dirent.first_cluster());
                            }
                            return Err(Error::NotADirectory);
                        }
                    }
                }
                _ => {
                    panic!("Unexpected path component!");
                }
            }
        }

        // Create a fake DirectoryEntry to represent the root directory
        Ok(Self {
            filename_len: 8,
            attributes: FATX_ATTR_DIRECTORY,
            filename_bytes: {
                let mut filename_bytes = [0u8; FATX_MAX_FILENAME_LEN];
                filename_bytes[0..4].copy_from_slice(b"root");
                filename_bytes
            },
            first_cluster: fs.root_cluster,
            file_size: 0,
            modified_date: 0,
            modified_time: 0,
            created_time: 0,
            created_date: 0,
            accessed_time: 0,
            accessed_date: 0,
            epoch: fs.epoch,
        })
    }

    pub(crate) fn kind(&self) -> DirectoryEntryKind {
        match self.filename_len {
            FATX_END_OF_DIR_MARKER => DirectoryEntryKind::EndOfDirectory,
            FATX_END_OF_DIR_MARKER2 => DirectoryEntryKind::EndOfDirectory,
            FATX_DELETED_FILE_MARKER => DirectoryEntryKind::Deleted,
            _ => DirectoryEntryKind::Valid,
        }
    }

    pub fn file_name(&self) -> String {
        assert_eq!(self.kind(), DirectoryEntryKind::Valid);
        let bytes = &self.filename_bytes[..self.filename_len as usize];
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    pub fn file_size(&self) -> u32 {
        self.file_size
    }

    pub fn is_directory(&self) -> bool {
        !self.is_volume() && self.attributes & FATX_ATTR_DIRECTORY == FATX_ATTR_DIRECTORY
    }

    pub fn is_file(&self) -> bool {
        !self.is_volume() && !self.is_directory()
    }

    pub fn is_hidden(&self) -> bool {
        self.attributes & FATX_ATTR_HIDDEN == FATX_ATTR_HIDDEN
    }

    pub fn is_read_only(&self) -> bool {
        self.attributes & FATX_ATTR_READ_ONLY == FATX_ATTR_READ_ONLY
    }

    pub fn is_volume(&self) -> bool {
        self.attributes & FATX_ATTR_VOLUME == FATX_ATTR_VOLUME
    }

    pub fn is_system(&self) -> bool {
        self.attributes & FATX_ATTR_SYSTEM == FATX_ATTR_SYSTEM
    }

    pub fn created(&self) -> DateTime {
        DateTime::from_fatx_encoding(self.created_date, self.created_time, self.epoch)
    }

    pub fn modified(&self) -> DateTime {
        DateTime::from_fatx_encoding(self.modified_date, self.modified_time, self.epoch)
    }

    pub fn accessed(&self) -> DateTime {
        DateTime::from_fatx_encoding(self.accessed_date, self.accessed_time, self.epoch)
    }

    pub(crate) fn first_cluster(&self) -> ClusterId {
        self.first_cluster
    }
}

pub(crate) struct DirectoryEntryIterator {
    cluster: ClusterId,
    entry: i64,
    finished: bool,
}

impl DirectoryEntryIterator {
    pub(crate) fn new(cluster: ClusterId) -> Self {
        Self {
            cluster,
            entry: -1,
            finished: false,
        }
    }

    pub(crate) fn next(&mut self, fs: &mut FatxFs) -> Option<Result<DirectoryEntry, Error>> {
        if self.finished {
            return None;
        }

        self.entry += 1;

        if (self.entry >= 0) && (self.entry as u64 >= fs.num_entries_per_cluster) {
            // Advance to next cluster
            let fat_entry = fs.fat.entry(self.cluster);
            match fat_entry {
                Err(err) => {
                    self.finished = true;
                    return Some(Err(err));
                }
                Ok(FatEntry::Data(next_cluster)) => {
                    self.cluster = next_cluster as ClusterId;
                    self.entry = 0;
                }
                _ => {
                    self.finished = true;
                    return Some(Err(Error::InvalidClusterChain));
                }
            }
        }

        // Fetch next entry
        if let Err(seek_error) = fs.seek_cluster(
            self.cluster,
            self.entry as u64 * FATX_DIRENT_SIZE_BYTES,
        ) {
            return Some(Err(seek_error));
        }

        let epoch = fs.epoch;
        let result = match fs.variant {
            Variant::Xbox => RawDirentXbox::read_from_io(&mut fs.device_handle)
                .map(|raw| raw.decode(epoch))
                .map_err(Error::from),
            Variant::Xbox360 => RawDirentXbox360::read_from_io(&mut fs.device_handle)
                .map(|raw| raw.decode(epoch))
                .map_err(Error::from),
        };

        match result {
            Err(err) => {
                self.finished = true;
                Some(Err(err))
            }
            Ok(entry) => {
                if entry.kind() == DirectoryEntryKind::EndOfDirectory {
                    self.finished = true;
                }
                Some(Ok(entry))
            }
        }
    }
}

pub struct DirectoryEntryIntoIterator {
    pub fs: FatxFsHandle,
    entry_iter: DirectoryEntryIterator,
}

impl DirectoryEntryIntoIterator {
    pub fn new(handle: FatxFsHandle, cluster: ClusterId) -> Self {
        Self {
            fs: handle,
            entry_iter: DirectoryEntryIterator::new(cluster),
        }
    }
}

impl Iterator for DirectoryEntryIntoIterator {
    type Item = Result<DirectoryEntry, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.fs.with_lock(|fs| {
            loop {
                // Filter-in only valid directory entry kinds
                let item = self.entry_iter.next(fs);
                if let Some(Ok(dirent)) = item {
                    if let DirectoryEntryKind::Valid = dirent.kind() {
                        return Some(Ok(dirent));
                    } else {
                        continue;
                    }
                }
                return item;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xbox_raw_bytes() -> [u8; 64] {
        let mut buf = [0u8; 64];
        buf[0] = 4; // filename_len
        buf[1] = FATX_ATTR_DIRECTORY; // attributes
        buf[2..6].copy_from_slice(b"test");
        buf[0x2c..0x30].copy_from_slice(&5u32.to_le_bytes()); // first_cluster
        buf[0x30..0x34].copy_from_slice(&0u32.to_le_bytes()); // file_size
        // modified_time, modified_date, created_time, created_date,
        // accessed_time, accessed_date (time-then-date, LE)
        buf[0x38..0x3a].copy_from_slice(&0x0000u16.to_le_bytes()); // created_time
        buf[0x3a..0x3c].copy_from_slice(&encode_date(2005, 6, 15, 2000).to_le_bytes()); // created_date
        buf
    }

    fn xbox360_raw_bytes() -> [u8; 64] {
        let mut buf = [0u8; 64];
        buf[0] = 4; // filename_len
        buf[1] = FATX_ATTR_DIRECTORY; // attributes
        buf[2..6].copy_from_slice(b"test");
        buf[0x2c..0x30].copy_from_slice(&5u32.to_be_bytes()); // first_cluster
        buf[0x30..0x34].copy_from_slice(&0u32.to_be_bytes()); // file_size
        // created_date, created_time, modified_date, modified_time,
        // accessed_date, accessed_time (date-then-time, BE)
        buf[0x34..0x36].copy_from_slice(&encode_date(2005, 6, 15, 1980).to_be_bytes()); // created_date
        buf[0x36..0x38].copy_from_slice(&0x0000u16.to_be_bytes()); // created_time
        buf
    }

    fn encode_date(year: u16, month: u16, day: u16, epoch: u16) -> u16 {
        (((year - epoch) & 0x7f) << 9) | ((month & 0xf) << 5) | (day & 0x1f)
    }

    #[test]
    fn decode_xbox_dirent() {
        let raw = RawDirentXbox::read_from_bytes(&xbox_raw_bytes()).unwrap();
        let entry = raw.decode(2000);
        assert_eq!(entry.file_name(), "test");
        assert!(entry.is_directory());
        assert_eq!(entry.first_cluster(), 5);
        assert_eq!(entry.created().year(), 2005);
        assert_eq!(entry.created().month(), 6);
        assert_eq!(entry.created().day(), 15);
    }

    #[test]
    fn decode_xbox360_dirent() {
        let raw = RawDirentXbox360::read_from_bytes(&xbox360_raw_bytes()).unwrap();
        let entry = raw.decode(1980);
        assert_eq!(entry.file_name(), "test");
        assert!(entry.is_directory());
        assert_eq!(entry.first_cluster(), 5);
        assert_eq!(entry.created().year(), 2005);
        assert_eq!(entry.created().month(), 6);
        assert_eq!(entry.created().day(), 15);
    }
}
