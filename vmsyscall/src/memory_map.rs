//! MemoryMap handed over to the kernel
//!
//! copied from
//! https://github.com/rust-osdev/bootloader/blob/90f5b8910d146d6d489b70a6341d778253663cfa/src/bootinfo/memory_map.rs

use core::cmp::Ordering;
use core::fmt;
use core::ops::{Deref, DerefMut};

pub(crate) const PAGE_SIZE: u64 = 4096;

const MAX_MEMORY_MAP_SIZE: usize = 64;

/// A map of the physical memory regions of the underlying machine.
#[derive(Clone)]
#[repr(C)]
pub struct MemoryMap {
    entries: [MemoryRegion; MAX_MEMORY_MAP_SIZE],
    // u64 instead of usize so that the structure layout is platform
    // independent
    next_entry_index: u64,
}

#[doc(hidden)]
impl MemoryMap {
    pub const fn new() -> Self {
        MemoryMap {
            entries: [MemoryRegion::empty(); MAX_MEMORY_MAP_SIZE],
            next_entry_index: 0,
        }
    }

    pub fn set_region_type_usable(&mut self, region_type: MemoryRegionType) {
        self.iter_mut().for_each(|r| {
            if r.region_type == region_type {
                r.region_type = MemoryRegionType::Usable
            }
        });
    }

    pub fn add_region(&mut self, region: MemoryRegion) {
        if let Err(()) = self.entries.iter_mut().try_for_each(|last_region| {
            if last_region.region_type == region.region_type
                && last_region.range.end_frame_number >= region.range.start_frame_number
                && last_region.range.end_frame_number <= region.range.end_frame_number
            {
                last_region.range.end_frame_number = region.range.end_frame_number;
                return Err(());
            }
            Ok(())
        }) {
            return;
        };

        assert!(
            self.next_entry_index() < MAX_MEMORY_MAP_SIZE,
            "too many memory regions in memory map"
        );

        self.entries[self.next_entry_index()] = region;
        self.next_entry_index += 1;
        self.sort();
    }

    pub fn mark_allocated_region(&mut self, region: MemoryRegion) {
        let mut region = region;
        for r in self.iter_mut() {
            // New region inside region of same type
            if r.region_type == region.region_type
                && r.range.start_frame_number <= region.range.start_frame_number
                && r.range.end_frame_number >= region.range.end_frame_number
            {
                return;
            }

            // New region extends old region
            if r.region_type == region.region_type
                && r.range.start_frame_number <= region.range.start_frame_number
                && r.range.end_frame_number > region.range.start_frame_number
                && r.range.end_frame_number <= region.range.end_frame_number
            {
                region.range.start_frame_number = r.range.end_frame_number;
            }

            if region.range.start_frame_number >= r.range.end_frame_number {
                continue;
            }
            if region.range.end_frame_number <= r.range.start_frame_number {
                continue;
            }

            if r.region_type != MemoryRegionType::Usable {
                panic!(
                    "region {:x?} overlaps with non-usable region {:x?}",
                    region, r
                );
            }

            match region
                .range
                .start_frame_number
                .cmp(&r.range.start_frame_number)
            {
                Ordering::Equal => {
                    if region.range.end_frame_number < r.range.end_frame_number {
                        // Case: (r = `r`, R = `region`)
                        // ----rrrrrrrrrrr----
                        // ----RRRR-----------
                        r.range.start_frame_number = region.range.end_frame_number;
                        self.add_region(region);
                    } else {
                        // Case: (r = `r`, R = `region`)
                        // ----rrrrrrrrrrr----
                        // ----RRRRRRRRRRRRRR-
                        *r = region;
                    }
                }
                Ordering::Greater => {
                    if region.range.end_frame_number < r.range.end_frame_number {
                        // Case: (r = `r`, R = `region`)
                        // ----rrrrrrrrrrr----
                        // ------RRRR---------
                        let mut behind_r = *r;
                        behind_r.range.start_frame_number = region.range.end_frame_number;
                        r.range.end_frame_number = region.range.start_frame_number;
                        self.add_region(behind_r);
                        self.add_region(region);
                    } else {
                        // Case: (r = `r`, R = `region`)
                        // ----rrrrrrrrrrr----
                        // -----------RRRR---- or
                        // -------------RRRR--
                        r.range.end_frame_number = region.range.start_frame_number;
                        self.add_region(region);
                    }
                }
                _ => {
                    // Case: (r = `r`, R = `region`)
                    // ----rrrrrrrrrrr----
                    // --RRRR-------------
                    r.range.start_frame_number = region.range.end_frame_number;
                    self.add_region(region);
                }
            }

            return;
        }
        panic!(
            "region {:x?} is not a usable memory region\n{:#?}",
            region, self
        );
    }

    pub fn sort(&mut self) {
        self.entries.sort_unstable_by(|r1, r2| {
            if r1.range.is_empty() {
                Ordering::Greater
            } else if r2.range.is_empty() {
                Ordering::Less
            } else {
                let ordering = r1
                    .range
                    .start_frame_number
                    .cmp(&r2.range.start_frame_number);

                if ordering == Ordering::Equal {
                    r1.range.end_frame_number.cmp(&r2.range.end_frame_number)
                } else {
                    ordering
                }
            }
        });
        if let Some(first_zero_index) = self.entries.iter().position(|r| r.range.is_empty()) {
            self.next_entry_index = first_zero_index as u64;
        }
    }

    fn next_entry_index(&self) -> usize {
        self.next_entry_index as usize
    }
}

impl Default for MemoryMap {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for MemoryMap {
    type Target = [MemoryRegion];

    fn deref(&self) -> &Self::Target {
        &self.entries[0..self.next_entry_index()]
    }
}

impl DerefMut for MemoryMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let next_index = self.next_entry_index();
        &mut self.entries[0..next_index]
    }
}

impl fmt::Debug for MemoryMap {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Represents a region of physical memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct MemoryRegion {
    /// The range of frames that belong to the region.
    pub range: FrameRange,
    /// The type of the region.
    pub region_type: MemoryRegionType,
}

#[doc(hidden)]
impl MemoryRegion {
    pub const fn empty() -> Self {
        MemoryRegion {
            range: FrameRange {
                start_frame_number: 0,
                end_frame_number: 0,
            },
            region_type: MemoryRegionType::Empty,
        }
    }
}

/// A range of frames with an exclusive upper bound.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct FrameRange {
    /// The frame _number_ of the first 4KiB frame in the region.
    ///
    /// This convert this frame number to a physical address, multiply it with the
    /// page size (4KiB).
    pub start_frame_number: u64,
    /// The frame _number_ of the first 4KiB frame that does no longer belong to the region.
    ///
    /// This convert this frame number to a physical address, multiply it with the
    /// page size (4KiB).
    pub end_frame_number: u64,
}

impl FrameRange {
    /// Create a new FrameRange from the passed start_addr and end_addr.
    ///
    /// The end_addr is exclusive.
    pub fn new(start_addr: u64, end_addr: u64) -> Self {
        let last_byte = end_addr - 1;
        FrameRange {
            start_frame_number: start_addr / PAGE_SIZE,
            end_frame_number: (last_byte / PAGE_SIZE) + 1,
        }
    }

    /// Returns true if the frame range contains no frames.
    pub fn is_empty(&self) -> bool {
        self.start_frame_number == self.end_frame_number
    }

    /// Length of the frame range
    pub fn len(&self) -> u64 {
        self.end_frame_number - self.start_frame_number
    }

    /// Returns the physical start address of the memory region.
    pub fn start_addr(&self) -> u64 {
        self.start_frame_number * PAGE_SIZE
    }

    /// Returns the physical end address of the memory region.
    pub fn end_addr(&self) -> u64 {
        self.end_frame_number * PAGE_SIZE
    }
}

impl fmt::Debug for FrameRange {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "FrameRange({:#x}..{:#x})",
            self.start_addr(),
            self.end_addr()
        )
    }
}

/// Represents possible types for memory regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum MemoryRegionType {
    /// Unused memory, can be freely used by the kernel.
    Usable,
    /// Memory that is already in use.
    InUse,
    /// Memory reserved by the hardware. Not usable.
    Reserved,
    /// ACPI reclaimable memory
    AcpiReclaimable,
    /// ACPI NVS memory
    AcpiNvs,
    /// Area containing bad memory
    BadMemory,
    /// Memory used for loading the kernel.
    Kernel,
    /// Memory used for loading the ELF app.
    App,
    /// Memory used by the bootloader.
    Bootloader,
    /// Frame at address zero.
    ///
    /// (shouldn't be used because it's easy to make mistakes related to null pointers)
    FrameZero,
    /// An empty region with size 0
    Empty,
    /// Additional variant to ensure that we can add more variants in the future without
    /// breaking backwards compatibility.
    #[doc(hidden)]
    NonExhaustive,
}

extern "C" {
    fn _improper_ctypes_check(_boot_info: MemoryMap);
}
