use std::io::Write;
use std::path::{Component, Path};
use std::time::SystemTime;

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
    fn decode(&self, epoch: u16, location: DirentLocation) -> DirectoryEntry {
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
            location: Some(location),
        }
    }

    fn encode(entry: &DirectoryEntry) -> Self {
        Self {
            filename_len: entry.filename_len,
            attributes: entry.attributes,
            filename_bytes: entry.filename_bytes,
            first_cluster: entry.first_cluster.into(),
            file_size: entry.file_size.into(),
            modified_time: entry.modified_time.into(),
            modified_date: entry.modified_date.into(),
            created_time: entry.created_time.into(),
            created_date: entry.created_date.into(),
            accessed_time: entry.accessed_time.into(),
            accessed_date: entry.accessed_date.into(),
        }
    }
}

impl RawDirentXbox360 {
    fn decode(&self, epoch: u16, location: DirentLocation) -> DirectoryEntry {
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
            location: Some(location),
        }
    }

    fn encode(entry: &DirectoryEntry) -> Self {
        Self {
            filename_len: entry.filename_len,
            attributes: entry.attributes,
            filename_bytes: entry.filename_bytes,
            first_cluster: entry.first_cluster.into(),
            file_size: entry.file_size.into(),
            modified_time: entry.modified_time.into(),
            modified_date: entry.modified_date.into(),
            created_time: entry.created_time.into(),
            created_date: entry.created_date.into(),
            accessed_time: entry.accessed_time.into(),
            accessed_date: entry.accessed_date.into(),
        }
    }
}

/// Where a `DirectoryEntry` lives on disk: which cluster of its parent
/// directory, and which 64-byte slot within that cluster. Needed to write
/// the entry back after mutating it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirentLocation {
    pub(crate) cluster: ClusterId,
    pub(crate) entry_index: u64,
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
    /// Where this entry lives on disk. `None` only for the synthetic root
    /// entry, which isn't a real on-disk record and is never rewritten.
    location: Option<DirentLocation>,
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
            location: None,
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

    pub(crate) fn set_file_size(&mut self, size: u32) {
        self.file_size = size;
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

    pub(crate) fn attributes(&self) -> u8 {
        self.attributes
    }

    pub(crate) fn set_name(&mut self, name: &str) -> Result<(), Error> {
        let (filename_bytes, filename_len) = encode_filename(name)?;
        self.filename_bytes = filename_bytes;
        self.filename_len = filename_len;
        Ok(())
    }

    pub(crate) fn set_modified(&mut self, dt: DateTime) {
        (self.modified_date, self.modified_time) = dt.to_fatx_encoding(self.epoch);
    }

    pub(crate) fn set_accessed(&mut self, dt: DateTime) {
        (self.accessed_date, self.accessed_time) = dt.to_fatx_encoding(self.epoch);
    }

    /// Swaps every field except `location` (each entry keeps its own disk
    /// slot) and `epoch` (filesystem-wide, not per-entry) -- used to
    /// implement `rename`'s `exchange` semantics.
    pub(crate) fn swap_content(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.filename_len, &mut other.filename_len);
        std::mem::swap(&mut self.attributes, &mut other.attributes);
        std::mem::swap(&mut self.filename_bytes, &mut other.filename_bytes);
        std::mem::swap(&mut self.first_cluster, &mut other.first_cluster);
        std::mem::swap(&mut self.file_size, &mut other.file_size);
        std::mem::swap(&mut self.modified_time, &mut other.modified_time);
        std::mem::swap(&mut self.modified_date, &mut other.modified_date);
        std::mem::swap(&mut self.created_time, &mut other.created_time);
        std::mem::swap(&mut self.created_date, &mut other.created_date);
        std::mem::swap(&mut self.accessed_time, &mut other.accessed_time);
        std::mem::swap(&mut self.accessed_date, &mut other.accessed_date);
    }

    /// Re-encodes this entry and writes it back to its on-disk slot.
    pub(crate) fn write_back(&self, fs: &mut FatxFs) -> Result<(), Error> {
        let location = self
            .location
            .expect("write_back called on an entry with no on-disk location");
        fs.seek_cluster(
            location.cluster,
            location.entry_index * FATX_DIRENT_SIZE_BYTES,
        )?;
        match fs.variant {
            Variant::Xbox => RawDirentXbox::encode(self).write_to_io(&mut fs.device_handle)?,
            Variant::Xbox360 => {
                RawDirentXbox360::encode(self).write_to_io(&mut fs.device_handle)?
            }
        };
        Ok(())
    }

    /// Marks this entry's slot as deleted (a single `filename_len` byte
    /// write) without touching its data cluster chain -- the caller
    /// decides whether/when to free that separately (see `unlink`, and
    /// `FatxFs::rename`'s cross-directory move, which keeps the chain
    /// alive under a new dirent).
    pub(crate) fn mark_deleted(&mut self, fs: &mut FatxFs) -> Result<(), Error> {
        let location = self
            .location
            .expect("mark_deleted called on an entry with no on-disk location");
        fs.seek_cluster(
            location.cluster,
            location.entry_index * FATX_DIRENT_SIZE_BYTES,
        )?;
        fs.device_handle.write_all(&[FATX_DELETED_FILE_MARKER])?;
        self.filename_len = FATX_DELETED_FILE_MARKER;
        Ok(())
    }

    /// Fully removes this entry: marks its slot deleted, then frees its
    /// data cluster chain. Marking deleted *first* is a deliberate
    /// ordering change from libfatx (which frees the chain before marking
    /// deleted) -- that order leaves an unsafe window if interrupted,
    /// where a live-looking dirent points at already-freed clusters.
    pub(crate) fn unlink(mut self, fs: &mut FatxFs) -> Result<(), Error> {
        self.mark_deleted(fs)?;
        fs.free_cluster_chain(self.first_cluster)?;
        Ok(())
    }

    /// Low-level: allocates a directory slot for `name` under
    /// `parent_dir_first_cluster` and writes a dirent there with the given
    /// content. Does not allocate or free `first_cluster` -- callers that
    /// want a fresh file/directory should use `create_file`/
    /// `create_directory`, which allocate a cluster and roll it back on
    /// failure; `FatxFs::rename`'s cross-directory move calls this
    /// directly with an existing cluster chain that must NOT be freed on
    /// error (the source dirent still owns it).
    pub(crate) fn create(
        fs: &mut FatxFs,
        parent_dir_first_cluster: ClusterId,
        name: &str,
        attributes: u8,
        first_cluster: ClusterId,
        file_size: u32,
        timestamps: (DateTime, DateTime, DateTime),
    ) -> Result<Self, Error> {
        let (filename_bytes, filename_len) = encode_filename(name)?;

        let mut iter = DirectoryEntryIterator::new(parent_dir_first_cluster);
        let location = match iter.find_free_slot(fs)? {
            FreeSlot::Reuse(location) => location,
            FreeSlot::ExtendInCluster(location) => {
                write_end_of_dir_marker(
                    fs,
                    DirentLocation {
                        cluster: location.cluster,
                        entry_index: location.entry_index + 1,
                    },
                )?;
                location
            }
            FreeSlot::ExtendNewCluster {
                old_marker_location,
            } => {
                // The current cluster is full: its last slot holds the
                // stale end-of-directory marker. Allocate a new (zeroed)
                // cluster -- its first slot becomes the new marker "for
                // free" -- and reuse the old marker's slot for this entry.
                let new_cluster = fs.alloc_cluster(true)?;
                if let Err(err) = fs.attach_cluster(old_marker_location.cluster, new_cluster) {
                    let _ = fs.free_cluster_chain(new_cluster);
                    return Err(err);
                }
                old_marker_location
            }
        };

        let epoch = fs.epoch;
        let (created, modified, accessed) = timestamps;
        let (created_date, created_time) = created.to_fatx_encoding(epoch);
        let (modified_date, modified_time) = modified.to_fatx_encoding(epoch);
        let (accessed_date, accessed_time) = accessed.to_fatx_encoding(epoch);

        let entry = DirectoryEntry {
            filename_len,
            attributes,
            filename_bytes,
            first_cluster,
            file_size,
            modified_time,
            modified_date,
            created_time,
            created_date,
            accessed_time,
            accessed_date,
            epoch,
            location: Some(location),
        };
        entry.write_back(fs)?;
        Ok(entry)
    }

    /// Creates a new, empty file: allocates a fresh zeroed first cluster
    /// and stamps all three timestamps to "now". Rolls back the allocated
    /// cluster if slot allocation or the write fails.
    pub(crate) fn create_file(
        fs: &mut FatxFs,
        parent_dir_first_cluster: ClusterId,
        name: &str,
    ) -> Result<Self, Error> {
        Self::create_new(fs, parent_dir_first_cluster, name, 0)
    }

    /// Creates a new, empty directory. The fresh zeroed first cluster
    /// (from `create_new`) is already a valid empty directory (`0x00` ==
    /// end-of-directory at slot 0), so no extra initialization is needed.
    pub(crate) fn create_directory(
        fs: &mut FatxFs,
        parent_dir_first_cluster: ClusterId,
        name: &str,
    ) -> Result<Self, Error> {
        Self::create_new(fs, parent_dir_first_cluster, name, FATX_ATTR_DIRECTORY)
    }

    fn create_new(
        fs: &mut FatxFs,
        parent_dir_first_cluster: ClusterId,
        name: &str,
        attributes: u8,
    ) -> Result<Self, Error> {
        let first_cluster = fs.alloc_cluster(true)?;
        let now = DateTime::from_system_time(SystemTime::now());
        match Self::create(
            fs,
            parent_dir_first_cluster,
            name,
            attributes,
            first_cluster,
            0,
            (now, now, now),
        ) {
            Ok(entry) => Ok(entry),
            Err(err) => {
                let _ = fs.free_cluster_chain(first_cluster);
                Err(err)
            }
        }
    }
}

fn write_end_of_dir_marker(fs: &mut FatxFs, location: DirentLocation) -> Result<(), Error> {
    fs.seek_cluster(
        location.cluster,
        location.entry_index * FATX_DIRENT_SIZE_BYTES,
    )?;
    fs.device_handle.write_all(&[FATX_END_OF_DIR_MARKER])?;
    Ok(())
}

fn encode_filename(name: &str) -> Result<([u8; FATX_MAX_FILENAME_LEN], u8), Error> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() >= FATX_MAX_FILENAME_LEN || bytes.contains(&b'/') {
        return Err(Error::InvalidFilename);
    }
    let mut filename_bytes = [0u8; FATX_MAX_FILENAME_LEN];
    filename_bytes[..bytes.len()].copy_from_slice(bytes);
    Ok((filename_bytes, bytes.len() as u8))
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
        if let Err(seek_error) =
            fs.seek_cluster(self.cluster, self.entry as u64 * FATX_DIRENT_SIZE_BYTES)
        {
            return Some(Err(seek_error));
        }

        let epoch = fs.epoch;
        let location = DirentLocation {
            cluster: self.cluster,
            entry_index: self.entry as u64,
        };
        let result = match fs.variant {
            Variant::Xbox => RawDirentXbox::read_from_io(&mut fs.device_handle)
                .map(|raw| raw.decode(epoch, location))
                .map_err(Error::from),
            Variant::Xbox360 => RawDirentXbox360::read_from_io(&mut fs.device_handle)
                .map(|raw| raw.decode(epoch, location))
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

    /// Scans this directory (starting at wherever this iterator currently
    /// is) for a slot to reuse when creating a new entry: the first
    /// deleted entry found, or (if none) the end-of-directory slot itself,
    /// which the caller must either overwrite in place (if there's room
    /// left in the cluster) or expand a new cluster to hold.
    pub(crate) fn find_free_slot(&mut self, fs: &mut FatxFs) -> Result<FreeSlot, Error> {
        loop {
            match self.next(fs) {
                None => return Err(Error::InvalidClusterChain),
                Some(Err(err)) => return Err(err),
                Some(Ok(dirent)) => {
                    let location = dirent
                        .location
                        .expect("entries yielded by the iterator always have a location");
                    match dirent.kind() {
                        DirectoryEntryKind::Deleted => return Ok(FreeSlot::Reuse(location)),
                        DirectoryEntryKind::EndOfDirectory => {
                            return Ok(if location.entry_index + 1 < fs.num_entries_per_cluster {
                                FreeSlot::ExtendInCluster(location)
                            } else {
                                FreeSlot::ExtendNewCluster {
                                    old_marker_location: location,
                                }
                            });
                        }
                        DirectoryEntryKind::Valid => continue,
                    }
                }
            }
        }
    }
}

/// Where to write a new directory entry, as determined by
/// `DirectoryEntryIterator::find_free_slot`.
pub(crate) enum FreeSlot {
    /// Reuse a previously-deleted slot.
    Reuse(DirentLocation),
    /// The end-of-directory marker can just shift forward one slot within
    /// the current cluster, vacating this slot.
    ExtendInCluster(DirentLocation),
    /// The current cluster is full: `old_marker_location` is its last slot,
    /// currently holding the stale end-of-directory marker. The caller
    /// must allocate and attach a new (zeroed) cluster -- whose first slot
    /// becomes the new marker for free -- and then reuse
    /// `old_marker_location` for the new entry.
    ExtendNewCluster { old_marker_location: DirentLocation },
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

    fn dummy_location() -> DirentLocation {
        DirentLocation {
            cluster: 1,
            entry_index: 0,
        }
    }

    #[test]
    fn decode_xbox_dirent() {
        let raw = RawDirentXbox::read_from_bytes(&xbox_raw_bytes()).unwrap();
        let entry = raw.decode(2000, dummy_location());
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
        let entry = raw.decode(1980, dummy_location());
        assert_eq!(entry.file_name(), "test");
        assert!(entry.is_directory());
        assert_eq!(entry.first_cluster(), 5);
        assert_eq!(entry.created().year(), 2005);
        assert_eq!(entry.created().month(), 6);
        assert_eq!(entry.created().day(), 15);
    }

    #[test]
    fn xbox_dirent_encode_round_trips_against_fixture() {
        let bytes = xbox_raw_bytes();
        let raw = RawDirentXbox::read_from_bytes(&bytes).unwrap();
        let entry = raw.decode(2000, dummy_location());
        let re_encoded = RawDirentXbox::encode(&entry);
        assert_eq!(re_encoded.as_bytes(), &bytes[..]);
    }

    #[test]
    fn xbox360_dirent_encode_round_trips_against_fixture() {
        let bytes = xbox360_raw_bytes();
        let raw = RawDirentXbox360::read_from_bytes(&bytes).unwrap();
        let entry = raw.decode(1980, dummy_location());
        let re_encoded = RawDirentXbox360::encode(&entry);
        assert_eq!(re_encoded.as_bytes(), &bytes[..]);
    }

    #[test]
    fn encode_filename_accepts_valid_names() {
        let (bytes, len) = encode_filename("test.txt").unwrap();
        assert_eq!(len, 8);
        assert_eq!(&bytes[..8], b"test.txt");
        assert!(bytes[8..].iter().all(|&b| b == 0));
    }

    #[test]
    fn encode_filename_rejects_invalid_names() {
        assert!(matches!(encode_filename(""), Err(Error::InvalidFilename)));
        assert!(matches!(
            encode_filename(&"a".repeat(42)),
            Err(Error::InvalidFilename)
        ));
        assert!(encode_filename(&"a".repeat(41)).is_ok());
        assert!(matches!(
            encode_filename("a/b"),
            Err(Error::InvalidFilename)
        ));
    }
}
