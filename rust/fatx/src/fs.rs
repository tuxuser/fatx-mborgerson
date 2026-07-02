use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use crate::datetime::{FATX_EPOCH_XBOX, FATX_EPOCH_XBOX360};
use crate::dir::{DirectoryEntry, DirectoryEntryIntoIterator};
use crate::error::Error;
use crate::fat::{ClusterId, Fat};
use crate::file::File;
use crate::partition::{
    DEFAULT_PARTITION_LAYOUT, PartitionMapEntry, XBOX360_RETAIL_COMPAT_PARTITION,
    XBOX360_RETAIL_DATA_PARTITION_OFFSET_BYTES, Xbox360DevkitPartition,
};

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
        }
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
        }
    }
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
        // Open device
        let mut device_handle = std::fs::File::open(&config.device_path)?;

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
        let num_entries_per_cluster: u64 = num_bytes_per_cluster / crate::dir::FATX_DIRENT_SIZE_BYTES;

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

    pub(crate) fn stat<P: AsRef<Path>>(&mut self, path: P) -> Result<DirectoryEntry, Error> {
        DirectoryEntry::from_path(self, path)
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
}
