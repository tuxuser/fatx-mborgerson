pub struct PartitionMapEntry {
    pub letter: &'static str,
    pub offset_bytes: u64,
    pub size_bytes: u64,
}

pub const DEFAULT_PARTITION_LAYOUT: &[PartitionMapEntry] = &[
    PartitionMapEntry {
        letter: "x",
        offset_bytes: 0x00080000,
        size_bytes: 0x02ee00000,
    },
    PartitionMapEntry {
        letter: "y",
        offset_bytes: 0x2ee80000,
        size_bytes: 0x02ee00000,
    },
    PartitionMapEntry {
        letter: "z",
        offset_bytes: 0x5dc80000,
        size_bytes: 0x02ee00000,
    },
    PartitionMapEntry {
        letter: "c",
        offset_bytes: 0x8ca80000,
        size_bytes: 0x01f400000,
    },
    PartitionMapEntry {
        letter: "e",
        offset_bytes: 0xabe80000,
        size_bytes: 0x1312d6000,
    },
];

impl PartitionMapEntry {
    pub fn from_letter(letter: &str) -> Option<&PartitionMapEntry> {
        DEFAULT_PARTITION_LAYOUT
            .iter()
            .find(|&entry| entry.letter == letter)
    }
}

// Xbox 360 internal HDD (retail) partition layout. Offsets are fixed by the
// kernel; unlike the original Xbox layout, the "data" partition (user
// content, roughly the "E:" equivalent) has no fixed size -- it runs to the
// end of the drive, so its size is resolved at open time instead of being
// listed here. System Cache / Game Cache are SFCX (not FATX) and are out of
// scope for this crate.
//
// Source: free60.org/System-Software/Systems/FATX (retail HDD partition
// table), cross-checked against a real retail-partitioned drive.
pub const XBOX360_RETAIL_DATA_PARTITION_OFFSET_BYTES: u64 = 0x130eb0000;

pub const XBOX360_RETAIL_COMPAT_PARTITION: PartitionMapEntry = PartitionMapEntry {
    letter: "compat",
    offset_bytes: 0x120eb0000,
    size_bytes: 0x10000000,
};

/// Xbox 360 devkit HDDs store an 0x18-byte partition table at device offset
/// 0 instead of using fixed retail offsets. Each entry is a (sector offset,
/// sector count) pair, big-endian, to be multiplied by the 512-byte sector
/// size.
#[derive(Debug, Clone, Copy)]
pub enum Xbox360DevkitPartition {
    /// User content volume (roughly the retail "data" partition equivalent).
    Content,
    /// Xbox 360 dashboard/system volume.
    Dashboard,
}

impl Xbox360DevkitPartition {
    /// Byte offset into the devkit partition table of the (sector offset,
    /// sector count) pair for this partition.
    pub(crate) fn table_offset(self) -> u64 {
        match self {
            Xbox360DevkitPartition::Content => 0x8,
            Xbox360DevkitPartition::Dashboard => 0x10,
        }
    }
}
