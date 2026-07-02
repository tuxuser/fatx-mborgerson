use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use crate::datetime::{DateTime, FATX_EPOCH_XBOX, FATX_EPOCH_XBOX360};
use crate::dir::{
    DirectoryEntry, DirectoryEntryIntoIterator, DirectoryEntryIterator, DirectoryEntryKind,
};
use crate::error::Error;
use crate::fat::{ClusterId, Fat, FatEntry};
use crate::file::File;
use crate::partition::{
    DEFAULT_PARTITION_LAYOUT, PartitionMapEntry, XBOX360_RETAIL_COMPAT_PARTITION,
    XBOX360_RETAIL_DATA_PARTITION_OFFSET_BYTES, Xbox360DevkitPartition,
};
use crate::path::normalize_virtual_path;

use zerocopy::byteorder::{big_endian, little_endian};
use zerocopy::*;

const FATX_SIGNATURE: u32 = 0x58544146; // 'FATX' (little-endian) / 'XTAF' (big-endian)
const FATX_FAT_OFFSET_BYTES: u64 = 4096;
const FATX_FAT_RESERVED_ENTRIES_COUNT: u32 = 1;

/// Which console's on-disk FATX conventions to use: byte order, dirent
/// timestamp layout, and timestamp epoch all differ between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Xbox,
    Xbox360,
}

impl Variant {
    fn is_big_endian(self) -> bool {
        matches!(self, Variant::Xbox360)
    }

    pub(crate) fn epoch(self) -> u16 {
        match self {
            Variant::Xbox => FATX_EPOCH_XBOX,
            Variant::Xbox360 => FATX_EPOCH_XBOX360,
        }
    }
}

// The superblock, as it appears on disk (original Xbox, little-endian).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C, packed)]
struct SuperblockLe {
    signature: little_endian::U32,
    volume_id: little_endian::U32,
    num_sectors_per_cluster: little_endian::U32,
    root_cluster: little_endian::U32,
    unknown_0: little_endian::U16,
    padding: [u8; 4078],
}

// The superblock, as it appears on disk (Xbox 360, big-endian). Same field
// layout as the original Xbox, only the byte order differs.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Debug)]
#[repr(C, packed)]
struct SuperblockBe {
    signature: big_endian::U32,
    volume_id: big_endian::U32,
    num_sectors_per_cluster: big_endian::U32,
    root_cluster: big_endian::U32,
    unknown_0: big_endian::U16,
    padding: [u8; 4078],
}

#[derive(Debug)]
pub struct FatxFs {
    self_handle: Weak<Mutex<FatxFs>>,

    pub(crate) device_handle: std::fs::File,
    pub(crate) partition_offset_bytes: u64,
    pub(crate) partition_size_bytes: u64,
    pub(crate) num_clusters: u32,
    pub(crate) num_bytes_per_cluster: u64,
    pub(crate) num_entries_per_cluster: u64,
    pub(crate) root_cluster: u32,
    pub(crate) fat_offset_bytes: u64,
    pub(crate) cluster_offset_bytes: u64,
    pub(crate) fat: Fat,
    pub(crate) variant: Variant,
    pub(crate) epoch: u16,
}

#[derive(Clone, Copy)]
enum PartitionSize {
    Fixed(u64),
    /// The partition has no fixed size; it runs to the end of the device
    /// (the Xbox 360 retail "data" partition).
    ToEndOfDevice,
}

#[derive(Clone, Copy)]
enum PartitionSource {
    Fixed(u64, PartitionSize),
    /// Read from the devkit partition table at device offset 0.
    Xbox360DevkitTable(Xbox360DevkitPartition),
}

pub struct FatxFsConfig {
    device_path: String,
    partition_source: PartitionSource,
    num_bytes_per_sector: u64,
    variant: Variant,
    read_write: bool,
}

impl FatxFsConfig {
    pub fn new(device_path: String) -> Self {
        let partition = &DEFAULT_PARTITION_LAYOUT[3];
        Self {
            device_path,
            partition_source: PartitionSource::Fixed(
                partition.offset_bytes,
                PartitionSize::Fixed(partition.size_bytes),
            ),
            num_bytes_per_sector: 512,
            variant: Variant::Xbox,
            read_write: false,
        }
    }

    /// Opens the device for writing as well as reading. Every constructor
    /// defaults to read-only; call this to opt in to write support.
    pub fn writable(mut self) -> Self {
        self.read_write = true;
        self
    }

    pub fn drive_letter(mut self, letter: &str) -> Self {
        let partition_info =
            PartitionMapEntry::from_letter(letter).expect("invalid partition letter");
        self.partition_source = PartitionSource::Fixed(
            partition_info.offset_bytes,
            PartitionSize::Fixed(partition_info.size_bytes),
        );
        self
    }

    /// Xbox 360 retail HDD, user content ("data") partition -- roughly the
    /// "E:" equivalent. It has no fixed size; it runs to the end of the
    /// drive, so the size is resolved when the device is opened.
    pub fn xbox360_retail_data(device_path: String) -> Self {
        Self {
            device_path,
            partition_source: PartitionSource::Fixed(
                XBOX360_RETAIL_DATA_PARTITION_OFFSET_BYTES,
                PartitionSize::ToEndOfDevice,
            ),
            num_bytes_per_sector: 512,
            variant: Variant::Xbox360,
            read_write: false,
        }
    }

    /// Xbox 360 retail HDD, original Xbox backwards-compatibility partition.
    pub fn xbox360_retail_compat(device_path: String) -> Self {
        let partition = &XBOX360_RETAIL_COMPAT_PARTITION;
        Self {
            device_path,
            partition_source: PartitionSource::Fixed(
                partition.offset_bytes,
                PartitionSize::Fixed(partition.size_bytes),
            ),
            num_bytes_per_sector: 512,
            variant: Variant::Xbox360,
            read_write: false,
        }
    }

    /// Xbox 360 devkit HDD. Unlike retail, devkits store their partition
    /// table on-disk instead of using fixed offsets, so it's read when the
    /// device is opened.
    pub fn xbox360_devkit(device_path: String, partition: Xbox360DevkitPartition) -> Self {
        Self {
            device_path,
            partition_source: PartitionSource::Xbox360DevkitTable(partition),
            num_bytes_per_sector: 512,
            variant: Variant::Xbox360,
            read_write: false,
        }
    }

    /// Opens a FATX filesystem starting at the very beginning of the given
    /// device/file, running for a fixed size, with no partition table
    /// involved. Useful for raw disk images or loopback files that aren't
    /// full Xbox/Xbox 360 HDDs.
    pub fn whole_device(device_path: String, size_bytes: u64, variant: Variant) -> Self {
        Self {
            device_path,
            partition_source: PartitionSource::Fixed(0, PartitionSize::Fixed(size_bytes)),
            num_bytes_per_sector: 512,
            variant,
            read_write: false,
        }
    }
}

/// Splits a virtual path into its parent directory and leaf name, e.g.
/// "/foo/bar" -> ("/foo", "bar"). Errors on the root path itself, which has
/// no parent.
fn split_parent_and_name<P: AsRef<Path>>(path: P) -> Result<(PathBuf, String), Error> {
    let normalized = normalize_virtual_path(path);
    let name = normalized
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(Error::NotADirectory)?
        .to_string();
    let parent = normalized
        .parent()
        .ok_or(Error::NotADirectory)?
        .to_path_buf();
    Ok((parent, name))
}

/// Reads a (sector offset, sector count) pair from the Xbox 360 devkit
/// partition table at device offset 0 and converts it to a byte
/// offset/size. The table is big-endian, like the rest of the Xbox 360
/// on-disk format.
fn read_xbox360_devkit_partition_table_entry(
    device_handle: &mut std::fs::File,
    partition: Xbox360DevkitPartition,
) -> Result<(u64, u64), Error> {
    device_handle.seek(SeekFrom::Start(partition.table_offset()))?;
    let mut buf = [0u8; 8];
    device_handle.read_exact(&mut buf)?;
    let sector_offset = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as u64;
    let sector_count = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as u64;
    Ok((sector_offset * 512, sector_count * 512))
}

impl FatxFs {
    pub fn open_device(config: &FatxFsConfig) -> Result<FatxFsHandle, Error> {
        // Open device. Every config defaults to read-only; `.writable()`
        // opts in to opening for writes too.
        let mut device_handle = std::fs::OpenOptions::new()
            .read(true)
            .write(config.read_write)
            .open(&config.device_path)?;

        // Resolve the partition offset/size. For Xbox 360 devkits this
        // means reading the on-disk partition table; for the Xbox 360
        // retail "data" partition, the size runs to the end of the device.
        let (partition_offset_bytes, partition_size) = match config.partition_source {
            PartitionSource::Fixed(offset, size) => (offset, size),
            PartitionSource::Xbox360DevkitTable(partition) => {
                let (offset, size) =
                    read_xbox360_devkit_partition_table_entry(&mut device_handle, partition)?;
                (offset, PartitionSize::Fixed(size))
            }
        };
        let partition_size_bytes = match partition_size {
            PartitionSize::Fixed(size) => size,
            PartitionSize::ToEndOfDevice => {
                let device_len_bytes = device_handle.seek(SeekFrom::End(0))?;
                device_len_bytes - partition_offset_bytes
            }
        };

        // Partition offset and size validation
        if !partition_offset_bytes.is_multiple_of(config.num_bytes_per_sector) {
            return Err(Error::InvalidPartitionOffset);
        }
        if !partition_size_bytes.is_multiple_of(config.num_bytes_per_sector) {
            return Err(Error::InvalidPartitionSize);
        }

        device_handle.seek(SeekFrom::Start(partition_offset_bytes))?;

        // Read superblock. Original Xbox is little-endian, Xbox 360 is
        // big-endian; the field layout is otherwise identical.
        let (signature, num_sectors_per_cluster, root_cluster): (u32, u32, u32) =
            if config.variant.is_big_endian() {
                let superblock = SuperblockBe::read_from_io(&mut device_handle)?;
                (
                    superblock.signature.into(),
                    superblock.num_sectors_per_cluster.into(),
                    superblock.root_cluster.into(),
                )
            } else {
                let superblock = SuperblockLe::read_from_io(&mut device_handle)?;
                (
                    superblock.signature.into(),
                    superblock.num_sectors_per_cluster.into(),
                    superblock.root_cluster.into(),
                )
            };
        if signature != FATX_SIGNATURE {
            return Err(Error::InvalidFilesystemSignature);
        }

        // Cluster geometry
        let num_sectors_per_cluster = num_sectors_per_cluster as u64;
        if !(num_sectors_per_cluster.is_power_of_two() && num_sectors_per_cluster <= 1024) {
            return Err(Error::InvalidSectorsPerCluster);
        }
        let num_bytes_per_cluster = num_sectors_per_cluster * config.num_bytes_per_sector;
        let num_entries_per_cluster: u64 =
            num_bytes_per_cluster / crate::dir::FATX_DIRENT_SIZE_BYTES;

        // Calculate FAT size
        let fat_offset_bytes = partition_offset_bytes + FATX_FAT_OFFSET_BYTES;
        let num_fat_entries = (partition_size_bytes / num_bytes_per_cluster) as u32;
        if root_cluster >= num_fat_entries {
            log::error!("Root cluster of {} exceeds cluster limit", root_cluster);
            return Err(Error::InvalidRootCluster);
        }

        // FIXME: Make FAT management smarter
        let mut fat = Fat::new(num_fat_entries, config.variant.is_big_endian());
        device_handle.seek(SeekFrom::Start(fat_offset_bytes))?;
        device_handle.read_exact(&mut fat.fat_data)?;

        // Cluster geometry cont'd
        let cluster_offset_bytes = fat_offset_bytes + fat.fat_size_bytes;
        let num_clusters = ((partition_size_bytes - fat.fat_size_bytes - FATX_FAT_OFFSET_BYTES)
            / num_bytes_per_cluster
            + FATX_FAT_RESERVED_ENTRIES_COUNT as u64) as u32;

        let variant = config.variant;
        let epoch = variant.epoch();
        let fs = Arc::new_cyclic(move |weak_self| {
            Mutex::new(FatxFs {
                self_handle: weak_self.clone(),
                device_handle,
                partition_offset_bytes,
                partition_size_bytes,
                num_clusters,
                num_bytes_per_cluster,
                num_entries_per_cluster,
                root_cluster,
                fat_offset_bytes,
                cluster_offset_bytes,
                fat,
                variant,
                epoch,
            })
        });

        Ok(FatxFsHandle { fs })
    }

    fn handle(&self) -> FatxFsHandle {
        FatxFsHandle {
            fs: self.self_handle.upgrade().unwrap().clone(),
        }
    }

    pub(crate) fn cluster_to_byte_offset(&self, cluster: ClusterId) -> Result<u64, Error> {
        if cluster >= self.num_clusters + FATX_FAT_RESERVED_ENTRIES_COUNT {
            return Err(Error::InvalidClusterNumber);
        }

        let byte_offset: u64 = self.cluster_offset_bytes
            + (cluster - FATX_FAT_RESERVED_ENTRIES_COUNT) as u64 * self.num_bytes_per_cluster;
        debug_assert!(byte_offset < (self.partition_offset_bytes + self.partition_size_bytes));

        Ok(byte_offset)
    }

    pub(crate) fn seek_cluster(
        &mut self,
        cluster: ClusterId,
        offset_in_cluster: u64,
    ) -> Result<(), Error> {
        self.device_handle.seek(SeekFrom::Start(
            self.cluster_to_byte_offset(cluster)? + offset_in_cluster,
        ))?;
        Ok(())
    }

    /// Mutates the in-memory FAT entry at `index` and writes just the
    /// changed bytes through to disk immediately (no dirty-window caching
    /// -- the whole FAT is already resident in memory, so there's nothing
    /// to gain from deferring the write, and doing it eagerly avoids
    /// losing FAT changes on a crash).
    pub(crate) fn write_fat_entry(
        &mut self,
        index: ClusterId,
        value: FatEntry,
    ) -> Result<(), Error> {
        self.fat.set_entry(index, value)?;
        let entry_size = self.fat.entry_size_bytes();
        let offset = self.fat_offset_bytes + index as u64 * entry_size;
        let start = (index as u64 * entry_size) as usize;
        let end = start + entry_size as usize;
        self.device_handle.seek(SeekFrom::Start(offset))?;
        self.device_handle
            .write_all(&self.fat.fat_data[start..end])?;
        Ok(())
    }

    /// Finds a free cluster, immediately claims it (marks it end-of-chain),
    /// and optionally zero-fills its data region.
    pub(crate) fn alloc_cluster(&mut self, zero: bool) -> Result<ClusterId, Error> {
        let cluster = self.fat.find_free_cluster()?;
        self.write_fat_entry(cluster, FatEntry::End)?;
        if zero {
            self.seek_cluster(cluster, 0)?;
            let zeroes = vec![0u8; self.num_bytes_per_cluster as usize];
            self.device_handle.write_all(&zeroes)?;
        }
        Ok(cluster)
    }

    /// Appends `new` to the chain currently ending at `tail`.
    pub(crate) fn attach_cluster(&mut self, tail: ClusterId, new: ClusterId) -> Result<(), Error> {
        if !matches!(self.fat.entry(tail)?, FatEntry::End) {
            return Err(Error::InvalidClusterChain);
        }
        self.write_fat_entry(tail, FatEntry::Data(new))?;
        self.write_fat_entry(new, FatEntry::End)?;
        Ok(())
    }

    /// Frees every cluster in the chain starting at `first`. Reads each
    /// cluster's "next" pointer before freeing it, since freeing zeroes the
    /// entry and would otherwise destroy the pointer needed to continue.
    pub(crate) fn free_cluster_chain(&mut self, first: ClusterId) -> Result<(), Error> {
        let mut cluster = Some(first);
        while let Some(current) = cluster {
            let next = self.fat.next_in_chain(current)?;
            self.write_fat_entry(current, FatEntry::Available)?;
            cluster = next;
        }
        Ok(())
    }

    /// Flushes the underlying device's data to durable storage. Since
    /// every write in this crate is already write-through (no in-process
    /// buffering), this just delegates to the OS/device's own durability
    /// guarantee -- primarily useful for FUSE's `fsync`/`flush` callbacks.
    pub fn sync(&mut self) -> Result<(), Error> {
        self.device_handle.sync_data()?;
        Ok(())
    }

    pub(crate) fn stat<P: AsRef<Path>>(&mut self, path: P) -> Result<DirectoryEntry, Error> {
        DirectoryEntry::from_path(self, path)
    }

    pub(crate) fn create_file<P: AsRef<Path>>(&mut self, path: P) -> Result<File, Error> {
        let (parent, name) = split_parent_and_name(path)?;
        let parent_dirent = self.stat(&parent)?;
        if !parent_dirent.is_directory() {
            return Err(Error::NotADirectory);
        }
        if self.stat(parent.join(&name)).is_ok() {
            return Err(Error::AlreadyExists);
        }
        let dirent = DirectoryEntry::create_file(self, parent_dirent.first_cluster(), &name)?;
        Ok(File::new(self.handle(), dirent))
    }

    pub(crate) fn mkdir<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        let (parent, name) = split_parent_and_name(path)?;
        let parent_dirent = self.stat(&parent)?;
        if !parent_dirent.is_directory() {
            return Err(Error::NotADirectory);
        }
        if self.stat(parent.join(&name)).is_ok() {
            return Err(Error::AlreadyExists);
        }
        DirectoryEntry::create_directory(self, parent_dirent.first_cluster(), &name)?;
        Ok(())
    }

    pub(crate) fn rmdir<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        let dirent = self.stat(path)?;
        if !dirent.is_directory() {
            return Err(Error::NotADirectory);
        }

        let mut iter = DirectoryEntryIterator::new(dirent.first_cluster());
        loop {
            match iter.next(self) {
                None => break,
                Some(Err(err)) => return Err(err),
                Some(Ok(child)) => match child.kind() {
                    DirectoryEntryKind::Valid => return Err(Error::DirectoryNotEmpty),
                    DirectoryEntryKind::EndOfDirectory => break,
                    DirectoryEntryKind::Deleted => continue,
                },
            }
        }

        dirent.unlink(self)
    }

    pub(crate) fn unlink<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        let dirent = self.stat(path)?;
        if dirent.is_directory() {
            return Err(Error::IsADirectory);
        }
        dirent.unlink(self)
    }

    pub(crate) fn set_times<P: AsRef<Path>>(
        &mut self,
        path: P,
        accessed: Option<DateTime>,
        modified: Option<DateTime>,
    ) -> Result<(), Error> {
        let mut dirent = self.stat(path)?;
        if let Some(a) = accessed {
            dirent.set_accessed(a);
        }
        if let Some(m) = modified {
            dirent.set_modified(m);
        }
        dirent.write_back(self)
    }

    pub(crate) fn rename<P: AsRef<Path>>(
        &mut self,
        from: P,
        to: P,
        exchange: bool,
        no_replace: bool,
    ) -> Result<(), Error> {
        if exchange && no_replace {
            return Err(Error::InvalidRenameFlags);
        }

        let (from_parent, _from_name) = split_parent_and_name(&from)?;
        let from_dirent = self.stat(&from)?;
        let (to_parent, to_name) = split_parent_and_name(&to)?;
        let to_parent_dirent = self.stat(&to_parent)?;
        if !to_parent_dirent.is_directory() {
            return Err(Error::NotADirectory);
        }
        let to_existing = self.stat(to_parent.join(&to_name)).ok();

        if exchange {
            let to_dirent = to_existing.ok_or(Error::NotFound)?;
            return self.exchange_dirent_content(from_dirent, to_dirent);
        }

        if let Some(to_dirent) = to_existing {
            if no_replace {
                return Err(Error::AlreadyExists);
            }
            if to_dirent.is_directory() != from_dirent.is_directory() {
                return Err(Error::RenameTypeMismatch);
            }
            to_dirent.unlink(self)?;
        }

        if from_parent == to_parent {
            // Same directory: rewrite the filename in place rather than
            // allocating a new slot and deleting the old one.
            let mut from_dirent = from_dirent;
            from_dirent.set_name(&to_name)?;
            from_dirent.write_back(self)
        } else {
            self.move_dirent(from_dirent, to_parent_dirent.first_cluster(), &to_name)
        }
    }

    /// Allocates a slot for `dest_name` under `dest_parent_cluster` with
    /// `source`'s existing content (name aside), then deletes `source`'s
    /// old slot WITHOUT freeing its cluster chain -- the chain now belongs
    /// to the new dirent. Used for cross-directory renames, where a dirent
    /// can't literally change its on-disk slot location.
    fn move_dirent(
        &mut self,
        mut source: DirectoryEntry,
        dest_parent_cluster: ClusterId,
        dest_name: &str,
    ) -> Result<(), Error> {
        let timestamps = (source.created(), source.modified(), source.accessed());
        DirectoryEntry::create(
            self,
            dest_parent_cluster,
            dest_name,
            source.attributes(),
            source.first_cluster(),
            source.file_size(),
            timestamps,
        )?;
        source.mark_deleted(self)
    }

    /// Swaps two existing dirents' content in place (each keeps its own
    /// disk slot) -- implements `rename`'s `exchange` semantics.
    fn exchange_dirent_content(
        &mut self,
        mut a: DirectoryEntry,
        mut b: DirectoryEntry,
    ) -> Result<(), Error> {
        a.swap_content(&mut b);
        a.write_back(self)?;
        b.write_back(self)?;
        Ok(())
    }

    pub(crate) fn open<P: AsRef<Path>>(&mut self, path: P) -> Result<File, Error> {
        let dirent = self.stat(path)?;
        if dirent.is_file() {
            Ok(File::new(self.handle(), dirent))
        } else {
            Err(Error::IsADirectory)
        }
    }

    pub(crate) fn read_dir(&mut self, path: &str) -> Result<DirectoryEntryIntoIterator, Error> {
        let dirent = DirectoryEntry::from_path(self, path)?;
        if !dirent.is_directory() {
            return Err(Error::NotADirectory);
        }
        Ok(DirectoryEntryIntoIterator::new(
            self.handle(),
            dirent.first_cluster(),
        ))
    }
}

pub struct FatxFsHandle {
    pub(crate) fs: Arc<Mutex<FatxFs>>, // FIXME: Make private
}

impl FatxFsHandle {
    pub(crate) fn with_lock<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut FatxFs) -> T,
    {
        let mut fs = self.fs.lock().unwrap();
        f(&mut fs)
    }

    pub fn open(&mut self, path: &str) -> Result<File, Error> {
        self.with_lock(|fs| fs.open(path))
    }

    pub fn stat(&mut self, path: &str) -> Result<DirectoryEntry, Error> {
        self.with_lock(|fs| fs.stat(path))
    }

    pub fn read_dir(&mut self, path: &str) -> Result<DirectoryEntryIntoIterator, Error> {
        self.with_lock(|fs| fs.read_dir(path))
    }

    pub fn sync(&mut self) -> Result<(), Error> {
        self.with_lock(|fs| fs.sync())
    }

    pub fn create_file(&mut self, path: &str) -> Result<File, Error> {
        self.with_lock(|fs| fs.create_file(path))
    }

    pub fn mkdir(&mut self, path: &str) -> Result<(), Error> {
        self.with_lock(|fs| fs.mkdir(path))
    }

    pub fn rmdir(&mut self, path: &str) -> Result<(), Error> {
        self.with_lock(|fs| fs.rmdir(path))
    }

    pub fn unlink(&mut self, path: &str) -> Result<(), Error> {
        self.with_lock(|fs| fs.unlink(path))
    }

    pub fn rename(
        &mut self,
        from: &str,
        to: &str,
        exchange: bool,
        no_replace: bool,
    ) -> Result<(), Error> {
        self.with_lock(|fs| fs.rename(from, to, exchange, no_replace))
    }

    pub fn set_times(
        &mut self,
        path: &str,
        accessed: Option<DateTime>,
        modified: Option<DateTime>,
    ) -> Result<(), Error> {
        self.with_lock(|fs| fs.set_times(path, accessed, modified))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SECTORS_PER_CLUSTER: u32 = 1;
    const BYTES_PER_SECTOR: u64 = 512;

    fn write_u32(buf: &mut [u8], offset: usize, value: u32, big_endian: bool) {
        let bytes = if big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        buf[offset..offset + 4].copy_from_slice(&bytes);
    }

    /// Builds a minimal valid FATX/XTAF image in a temp file: a superblock
    /// (root cluster 1, `SECTORS_PER_CLUSTER` sectors/cluster), a zeroed FAT
    /// sized for `num_fat_entries`, and zeroed data clusters filling out the
    /// rest. An all-zero cluster is itself a valid empty directory (0x00 ==
    /// end-of-directory at slot 0), so cluster 1 needs no special content to
    /// serve as an empty root. Returns the open temp file and the total
    /// image size in bytes (what `FatxFsConfig::whole_device` expects).
    pub(crate) fn build_synthetic_image(
        variant: Variant,
        num_fat_entries: u32,
    ) -> (tempfile::NamedTempFile, u64) {
        let bytes_per_cluster = SECTORS_PER_CLUSTER as u64 * BYTES_PER_SECTOR;
        let big_endian = matches!(variant, Variant::Xbox360);

        let mut header = [0u8; 4096];
        write_u32(&mut header, 0x0, FATX_SIGNATURE, big_endian);
        write_u32(&mut header, 0x4, 0x12345678, big_endian);
        write_u32(&mut header, 0x8, SECTORS_PER_CLUSTER, big_endian);
        write_u32(&mut header, 0xc, 1, big_endian);

        // Root (cluster 1) occupies exactly one cluster until it needs to
        // grow, so its FAT entry must show it as allocated/terminated --
        // otherwise the first attempt to attach a second cluster to it
        // fails the "tail must be End" check in `attach_cluster`.
        let mut fat = Fat::new(num_fat_entries, big_endian);
        fat.set_entry(1, FatEntry::End).unwrap();
        let fat_size_bytes = fat.fat_size_bytes;
        let partition_size_bytes = num_fat_entries as u64 * bytes_per_cluster;
        assert!(
            partition_size_bytes > 4096 + fat_size_bytes,
            "synthetic image too small for header+FAT overhead"
        );

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&header).unwrap();
        file.write_all(&fat.fat_data).unwrap();
        let data_len = partition_size_bytes - 4096 - fat_size_bytes;
        file.write_all(&vec![0u8; data_len as usize]).unwrap();
        file.flush().unwrap();

        (file, partition_size_bytes)
    }

    /// Builds a synthetic image and opens it, returning both the handle and
    /// the (path, size, variant) needed to reopen it later -- used by tests
    /// that need to verify writes actually persisted to disk, not just to
    /// the in-memory `Fat`/`DirectoryEntry` state.
    pub(crate) fn open_synthetic_image(
        variant: Variant,
        num_fat_entries: u32,
    ) -> (FatxFsHandle, tempfile::NamedTempFile, u64) {
        let (file, size) = build_synthetic_image(variant, num_fat_entries);
        let config =
            FatxFsConfig::whole_device(file.path().to_str().unwrap().to_string(), size, variant)
                .writable();
        let handle = FatxFs::open_device(&config).unwrap();
        (handle, file, size)
    }

    #[test]
    fn alloc_attach_free_cluster_round_trip() {
        for variant in [Variant::Xbox, Variant::Xbox360] {
            let (handle, file, size) = open_synthetic_image(variant, 64);
            let (a, b) = handle.with_lock(|fs| {
                let a = fs.alloc_cluster(true).unwrap();
                let b = fs.alloc_cluster(true).unwrap();
                fs.attach_cluster(a, b).unwrap();
                assert_eq!(fs.fat.entry(a).unwrap(), FatEntry::Data(b));
                assert_eq!(fs.fat.entry(b).unwrap(), FatEntry::End);
                fs.free_cluster_chain(a).unwrap();
                assert_eq!(fs.fat.entry(a).unwrap(), FatEntry::Available);
                assert_eq!(fs.fat.entry(b).unwrap(), FatEntry::Available);
                (a, b)
            });
            drop(handle);

            // Reopen fresh -- the FAT writes must be visible on disk, not
            // just in the now-dropped in-memory state.
            let config = FatxFsConfig::whole_device(
                file.path().to_str().unwrap().to_string(),
                size,
                variant,
            );
            let reopened = FatxFs::open_device(&config).unwrap();
            reopened.with_lock(|fs| {
                assert_eq!(fs.fat.entry(a).unwrap(), FatEntry::Available);
                assert_eq!(fs.fat.entry(b).unwrap(), FatEntry::Available);
            });
        }
    }

    #[test]
    fn alloc_cluster_zero_fills_data() {
        for variant in [Variant::Xbox, Variant::Xbox360] {
            let (handle, _file, _size) = open_synthetic_image(variant, 64);
            handle.with_lock(|fs| {
                let cluster = fs.alloc_cluster(true).unwrap();
                fs.seek_cluster(cluster, 0).unwrap();
                let mut buf = vec![0xffu8; fs.num_bytes_per_cluster as usize];
                fs.device_handle.read_exact(&mut buf).unwrap();
                assert!(buf.iter().all(|&b| b == 0));
            });
        }
    }

    #[test]
    fn attach_cluster_requires_end_of_chain_tail() {
        let (handle, _file, _size) = open_synthetic_image(Variant::Xbox, 64);
        handle.with_lock(|fs| {
            let a = fs.alloc_cluster(false).unwrap();
            let b = fs.alloc_cluster(false).unwrap();
            let c = fs.alloc_cluster(false).unwrap();
            fs.attach_cluster(a, b).unwrap();
            // `a` is no longer End (it points at b), so attaching again
            // through it must fail rather than silently overwrite the link.
            assert!(matches!(
                fs.attach_cluster(a, c),
                Err(Error::InvalidClusterChain)
            ));
        });
    }

    fn run_directory_ops_scenario(variant: Variant) {
        let (mut handle, _file, _size) = open_synthetic_image(variant, 64);

        // create_file + stat + read_dir
        let file = handle.create_file("/hello.txt").unwrap();
        assert_eq!(file.file_size(), 0);
        let stat = handle.stat("/hello.txt").unwrap();
        assert!(stat.is_file());
        assert_eq!(stat.file_name(), "hello.txt");
        let names: Vec<String> = handle
            .read_dir("/")
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["hello.txt"]);

        // create_file on an existing path fails
        assert!(matches!(
            handle.create_file("/hello.txt"),
            Err(Error::AlreadyExists)
        ));

        // mkdir + nested create_file
        handle.mkdir("/sub").unwrap();
        let sub_stat = handle.stat("/sub").unwrap();
        assert!(sub_stat.is_directory());
        handle.create_file("/sub/inner.txt").unwrap();
        let inner_names: Vec<String> = handle
            .read_dir("/sub")
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(inner_names, vec!["inner.txt"]);

        // rmdir on a non-empty directory fails; unlink its child, then rmdir succeeds
        assert!(matches!(
            handle.rmdir("/sub"),
            Err(Error::DirectoryNotEmpty)
        ));
        handle.unlink("/sub/inner.txt").unwrap();
        handle.rmdir("/sub").unwrap();
        assert!(matches!(handle.stat("/sub"), Err(Error::NotFound)));

        // unlink reclaims the deleted slot for a later create
        handle.unlink("/hello.txt").unwrap();
        assert!(matches!(handle.stat("/hello.txt"), Err(Error::NotFound)));
        handle.create_file("/reused.txt").unwrap();
        let names: Vec<String> = handle
            .read_dir("/")
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["reused.txt"]);

        // Directory extension: root's cluster holds 8 entries (512-byte
        // cluster / 64-byte dirent); create enough files to force a second
        // cluster to be allocated and attached to the root's chain.
        for i in 0..10 {
            handle.create_file(&format!("/f{i}")).unwrap();
        }
        let count = handle.read_dir("/").unwrap().count();
        assert_eq!(count, 1 /* reused.txt */ + 10);

        // rename: same-directory
        handle
            .rename("/reused.txt", "/renamed.txt", false, false)
            .unwrap();
        assert!(matches!(handle.stat("/reused.txt"), Err(Error::NotFound)));
        assert!(handle.stat("/renamed.txt").unwrap().is_file());

        // rename: cross-directory move
        handle.mkdir("/dest").unwrap();
        handle
            .rename("/renamed.txt", "/dest/moved.txt", false, false)
            .unwrap();
        assert!(matches!(handle.stat("/renamed.txt"), Err(Error::NotFound)));
        assert!(handle.stat("/dest/moved.txt").unwrap().is_file());

        // rename: no_replace against an existing destination fails
        handle.create_file("/dest/moved2.txt").unwrap();
        assert!(matches!(
            handle.rename("/dest/moved.txt", "/dest/moved2.txt", false, true),
            Err(Error::AlreadyExists)
        ));

        // rename: default overwrite replaces the destination
        handle
            .rename("/dest/moved.txt", "/dest/moved2.txt", false, false)
            .unwrap();
        assert!(matches!(
            handle.stat("/dest/moved.txt"),
            Err(Error::NotFound)
        ));
        assert!(handle.stat("/dest/moved2.txt").unwrap().is_file());

        // rename: exchange swaps two existing files' content in place
        handle.create_file("/a.txt").unwrap();
        handle.create_file("/b.txt").unwrap();
        handle.rename("/a.txt", "/b.txt", true, false).unwrap();
        assert!(handle.stat("/a.txt").unwrap().is_file());
        assert!(handle.stat("/b.txt").unwrap().is_file());

        // rename: mismatched types rejected outside of exchange
        assert!(matches!(
            handle.rename("/a.txt", "/dest", false, false),
            Err(Error::RenameTypeMismatch)
        ));
    }

    #[test]
    fn directory_ops_xbox() {
        run_directory_ops_scenario(Variant::Xbox);
    }

    #[test]
    fn directory_ops_xbox360() {
        run_directory_ops_scenario(Variant::Xbox360);
    }

    fn run_file_write_scenario(variant: Variant) {
        let (mut handle, _file, _size) = open_synthetic_image(variant, 128);

        // Write spanning multiple 512-byte clusters (2 full + a partial one).
        let payload: Vec<u8> = (0..1280u32).map(|i| (i % 256) as u8).collect();
        let mut f = handle.create_file("/data.bin").unwrap();
        f.write_all(&payload).unwrap();
        assert_eq!(f.file_size(), 1280);
        drop(f);

        // Persistence: a freshly opened File sees the same content and size.
        let mut f = handle.open("/data.bin").unwrap();
        let mut read_back = Vec::new();
        f.read_to_end(&mut read_back).unwrap();
        assert_eq!(read_back, payload);
        assert_eq!(f.file_size(), 1280);
        drop(f);
        assert_eq!(handle.stat("/data.bin").unwrap().file_size(), 1280);

        // Hole-fill: seek past EOF and write -- the gap must read back as
        // zero, and the new tail data must be correct.
        let mut f = handle.open("/data.bin").unwrap();
        f.seek(SeekFrom::Start(2000)).unwrap();
        f.write_all(b"tail").unwrap();
        drop(f);
        let mut f = handle.open("/data.bin").unwrap();
        let mut full = Vec::new();
        f.read_to_end(&mut full).unwrap();
        assert_eq!(full.len(), 2004);
        assert_eq!(&full[..1280], &payload[..]);
        assert!(full[1280..2000].iter().all(|&b| b == 0));
        assert_eq!(&full[2000..2004], b"tail");
        drop(f);

        // Shrink: retained prefix is unchanged, size updates.
        let mut f = handle.open("/data.bin").unwrap();
        f.set_len(100).unwrap();
        drop(f);
        assert_eq!(handle.stat("/data.bin").unwrap().file_size(), 100);
        let mut f = handle.open("/data.bin").unwrap();
        let mut shrunk = Vec::new();
        f.read_to_end(&mut shrunk).unwrap();
        assert_eq!(shrunk, &payload[..100]);
        drop(f);

        // Shrunk-away clusters must be reclaimed: create enough new files
        // to require them again (proves the space was actually freed,
        // rather than just asserting a FAT entry directly).
        for i in 0..20 {
            handle.create_file(&format!("/reclaim{i}")).unwrap();
        }

        // Grow: new region reads back as zero.
        let mut f = handle.open("/data.bin").unwrap();
        f.set_len(300).unwrap();
        drop(f);
        let mut f = handle.open("/data.bin").unwrap();
        let mut grown = Vec::new();
        f.read_to_end(&mut grown).unwrap();
        assert_eq!(grown.len(), 300);
        assert_eq!(&grown[..100], &payload[..100]);
        assert!(grown[100..].iter().all(|&b| b == 0));
    }

    #[test]
    fn file_write_xbox() {
        run_file_write_scenario(Variant::Xbox);
    }

    #[test]
    fn file_write_xbox360() {
        run_file_write_scenario(Variant::Xbox360);
    }
}
