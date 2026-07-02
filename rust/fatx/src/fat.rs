use zerocopy::byteorder::big_endian;
use zerocopy::byteorder::little_endian;
use zerocopy::*;

use crate::error::Error;

#[derive(Debug, Clone, Copy)]
enum FatType {
    Type16,
    Type32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FatEntry {
    Available,
    Reserved,
    Bad,
    Media,
    End,
    Data(u32),
    Invalid,
}

impl FatEntry {
    /// Encodes this entry back into its on-disk 16-bit representation.
    /// `Invalid` is never intentionally written -- it can only be produced
    /// by decoding corrupt on-disk data, so encoding it is a logic error.
    fn encode_u16(self) -> u16 {
        match self {
            FatEntry::Available => 0x0000,
            FatEntry::Data(v) => v as u16,
            FatEntry::Reserved => 0xfff0,
            FatEntry::Bad => 0xfff7,
            FatEntry::Media => 0xfff8,
            FatEntry::End => 0xffff,
            FatEntry::Invalid => panic!("cannot encode FatEntry::Invalid"),
        }
    }

    fn encode_u32(self) -> u32 {
        match self {
            FatEntry::Available => 0x00000000,
            FatEntry::Data(v) => v,
            FatEntry::Reserved => 0xfffffff0,
            FatEntry::Bad => 0xfffffff7,
            FatEntry::Media => 0xfffffff8,
            FatEntry::End => 0xffffffff,
            FatEntry::Invalid => panic!("cannot encode FatEntry::Invalid"),
        }
    }
}

pub(crate) type FatEntryId = u32;
pub(crate) type ClusterId = u32;

impl From<u16> for FatEntry {
    fn from(value: u16) -> Self {
        match value {
            0x0000 => FatEntry::Available,
            0x0001..0xfff0 => FatEntry::Data(value as u32),
            0xfff0 => FatEntry::Reserved,
            0xfff7 => FatEntry::Bad,
            0xfff8 => FatEntry::Media,
            0xffff => FatEntry::End,
            _ => FatEntry::Invalid,
        }
    }
}

impl From<u32> for FatEntry {
    fn from(value: u32) -> Self {
        match value {
            0x00000000 => FatEntry::Available,
            0x00000001..0xfffffff0 => FatEntry::Data(value),
            0xfffffff0 => FatEntry::Reserved,
            0xfffffff7 => FatEntry::Bad,
            0xfffffff8 => FatEntry::Media,
            0xffffffff => FatEntry::End,
            _ => FatEntry::Invalid,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Fat {
    fat_type: FatType,
    big_endian: bool,
    num_fat_entries: u32,
    /// Rolling cursor for the next-fit free-cluster scan. Unlike libfatx's
    /// C `static` scan hint (shared/global across every open filesystem in
    /// the process), this is a proper per-instance field.
    next_free_hint: FatEntryId,
    pub(crate) fat_size_bytes: u64,
    pub(crate) fat_data: Vec<u8>, // FIXME: Smarter cache
}

impl Fat {
    pub(crate) fn new(num_fat_entries: u32, big_endian: bool) -> Self {
        // NOTE: this *MUST* be kept below the Cluster Reserved marker for FAT16
        let (fat_type, fat_size_bytes) = if num_fat_entries < 0xfff0 {
            (
                FatType::Type16,
                (num_fat_entries as u64 * 2).next_multiple_of(4096),
            )
        } else {
            (
                FatType::Type32,
                (num_fat_entries as u64 * 4).next_multiple_of(4096),
            )
        };

        let fat_data = vec![0u8; fat_size_bytes as usize];

        Self {
            fat_type,
            big_endian,
            num_fat_entries,
            next_free_hint: 2,
            fat_size_bytes,
            fat_data,
        }
    }

    pub(crate) fn entry_size_bytes(&self) -> u64 {
        match self.fat_type {
            FatType::Type16 => 2,
            FatType::Type32 => 4,
        }
    }

    pub(crate) fn entry(&self, index: FatEntryId) -> Result<FatEntry, Error> {
        match (self.fat_type, self.big_endian) {
            (FatType::Type16, false) => {
                let fat = <[little_endian::U16]>::ref_from_bytes_with_elems(
                    &self.fat_data[..],
                    (self.fat_size_bytes / 2) as usize,
                )
                .unwrap();
                let value: u16 = fat[index as usize].into();
                Ok(value.into())
            }
            (FatType::Type16, true) => {
                let fat = <[big_endian::U16]>::ref_from_bytes_with_elems(
                    &self.fat_data[..],
                    (self.fat_size_bytes / 2) as usize,
                )
                .unwrap();
                let value: u16 = fat[index as usize].into();
                Ok(value.into())
            }
            (FatType::Type32, false) => {
                let fat = <[little_endian::U32]>::ref_from_bytes_with_elems(
                    &self.fat_data[..],
                    (self.fat_size_bytes / 4) as usize,
                )
                .unwrap();
                let value: u32 = fat[index as usize].into();
                Ok(value.into())
            }
            (FatType::Type32, true) => {
                let fat = <[big_endian::U32]>::ref_from_bytes_with_elems(
                    &self.fat_data[..],
                    (self.fat_size_bytes / 4) as usize,
                )
                .unwrap();
                let value: u32 = fat[index as usize].into();
                Ok(value.into())
            }
        }
    }

    /// Writes `value` into the in-memory FAT only. The caller (`FatxFs`,
    /// which owns the device handle) is responsible for writing the
    /// changed bytes through to disk.
    pub(crate) fn set_entry(&mut self, index: FatEntryId, value: FatEntry) -> Result<(), Error> {
        match (self.fat_type, self.big_endian) {
            (FatType::Type16, false) => {
                let fat = <[little_endian::U16]>::mut_from_bytes_with_elems(
                    &mut self.fat_data[..],
                    (self.fat_size_bytes / 2) as usize,
                )
                .unwrap();
                fat[index as usize] = value.encode_u16().into();
            }
            (FatType::Type16, true) => {
                let fat = <[big_endian::U16]>::mut_from_bytes_with_elems(
                    &mut self.fat_data[..],
                    (self.fat_size_bytes / 2) as usize,
                )
                .unwrap();
                fat[index as usize] = value.encode_u16().into();
            }
            (FatType::Type32, false) => {
                let fat = <[little_endian::U32]>::mut_from_bytes_with_elems(
                    &mut self.fat_data[..],
                    (self.fat_size_bytes / 4) as usize,
                )
                .unwrap();
                fat[index as usize] = value.encode_u32().into();
            }
            (FatType::Type32, true) => {
                let fat = <[big_endian::U32]>::mut_from_bytes_with_elems(
                    &mut self.fat_data[..],
                    (self.fat_size_bytes / 4) as usize,
                )
                .unwrap();
                fat[index as usize] = value.encode_u32().into();
            }
        }
        Ok(())
    }

    /// Finds a free (`Available`) cluster via a next-fit scan starting from
    /// `next_free_hint`, wrapping around at `num_fat_entries`. Cluster
    /// numbers 0 and 1 are reserved, so the scan starts at 2. Does not mark
    /// the cluster as used -- that's a separate step so callers can
    /// distinguish "found" from "claimed".
    pub(crate) fn find_free_cluster(&mut self) -> Result<ClusterId, Error> {
        let start = self.next_free_hint.max(2);
        let mut index = start;
        loop {
            if matches!(self.entry(index)?, FatEntry::Available) {
                self.next_free_hint = if index + 1 < self.num_fat_entries {
                    index + 1
                } else {
                    2
                };
                return Ok(index);
            }
            index = if index + 1 < self.num_fat_entries {
                index + 1
            } else {
                2
            };
            if index == start {
                return Err(Error::FilesystemFull);
            }
        }
    }

    /// Returns the next cluster in the chain after `cluster`: `Some(next)`
    /// for a data link, `None` at the end of the chain, or an error if the
    /// entry is anything else (available/reserved/bad/media/invalid mid-
    /// chain is corruption). Reading this before overwriting `cluster`'s
    /// own entry is what lets `FatxFs::free_cluster_chain` walk a chain
    /// while freeing it.
    pub(crate) fn next_in_chain(&self, cluster: ClusterId) -> Result<Option<ClusterId>, Error> {
        match self.entry(cluster)? {
            FatEntry::Data(next) => Ok(Some(next)),
            FatEntry::End => Ok(None),
            _ => Err(Error::InvalidClusterChain),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // FAT16 (num_fat_entries < 0xfff0) and FAT32, each in both byte orders.
    fn variants() -> [(u32, bool); 4] {
        [(16, false), (16, true), (0x10000, false), (0x10000, true)]
    }

    #[test]
    fn find_free_cluster_starts_at_2() {
        for (n, be) in variants() {
            let mut fat = Fat::new(n, be);
            assert_eq!(fat.find_free_cluster().unwrap(), 2);
        }
    }

    #[test]
    fn find_free_cluster_skips_occupied_and_uses_hint() {
        for (n, be) in variants() {
            let mut fat = Fat::new(n, be);
            let a = fat.find_free_cluster().unwrap();
            fat.set_entry(a, FatEntry::End).unwrap();
            let b = fat.find_free_cluster().unwrap();
            assert_ne!(a, b);
            assert!(b > a, "hint should avoid rescanning from the start");
        }
    }

    #[test]
    fn find_free_cluster_wraps_around() {
        for (n, be) in variants() {
            let mut fat = Fat::new(n, be);
            // Occupy every cluster, then free only cluster 2, and push the
            // scan hint to the last cluster so the next allocation must
            // wrap around past num_fat_entries to find it.
            for c in 2..n {
                fat.set_entry(c, FatEntry::End).unwrap();
            }
            fat.set_entry(2, FatEntry::Available).unwrap();
            fat.next_free_hint = n - 1;
            assert_eq!(fat.find_free_cluster().unwrap(), 2);
        }
    }

    #[test]
    fn find_free_cluster_reports_full() {
        let mut fat = Fat::new(4, false);
        for _ in 2..4 {
            let c = fat.find_free_cluster().unwrap();
            fat.set_entry(c, FatEntry::End).unwrap();
        }
        assert!(matches!(
            fat.find_free_cluster(),
            Err(Error::FilesystemFull)
        ));
    }

    #[test]
    fn set_entry_round_trips() {
        for (n, be) in variants() {
            let mut fat = Fat::new(n, be);
            for value in [FatEntry::Available, FatEntry::Data(5), FatEntry::End] {
                fat.set_entry(2, value).unwrap();
                assert_eq!(fat.entry(2).unwrap(), value);
            }
        }
    }

    #[test]
    fn next_in_chain_cases() {
        let mut fat = Fat::new(16, false);
        fat.set_entry(2, FatEntry::Data(3)).unwrap();
        fat.set_entry(3, FatEntry::End).unwrap();
        fat.set_entry(4, FatEntry::Available).unwrap();

        assert_eq!(fat.next_in_chain(2).unwrap(), Some(3));
        assert_eq!(fat.next_in_chain(3).unwrap(), None);
        assert!(matches!(
            fat.next_in_chain(4),
            Err(Error::InvalidClusterChain)
        ));
    }
}
