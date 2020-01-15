#[macro_use]
pub mod serial;
pub mod asm;
pub mod gdt;
pub mod interrupts;
pub mod mem;
mod start_e820;
pub mod structures;
pub mod syscall;

use crate::memory::BootInfoFrameAllocator;
use alloc::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use vmbootspec::layout::{
    PDPTE_OFFSET_START, PHYSICAL_MEMORY_OFFSET, USER_STACK_OFFSET, USER_STACK_SIZE, USER_TLS_OFFSET,
};
use vmbootspec::{BootInfo, MemoryRegionType};

use crate::arch::x86_64::structures::paging::{
    mapper::MapToError, FrameAllocator, Mapper, OffsetPageTable, Page, PageTableFlags, Size4KiB,
};

pub use x86_64::{PhysAddr, VirtAddr};

use x86_64::registers::control::Cr3;
use xmas_elf::program::{self, ProgramHeader64};

const PAGESIZE: usize = 4096;
pub fn pagesize() -> usize {
    PAGESIZE
}

pub const HEAP_START: usize = 0x4E43_0000_0000;
pub const HEAP_SIZE: usize = 100 * 1024; // 100 KiB
pub const STACK_START: usize = 0x4848_0000_0000;
pub const STACK_SIZE: usize = 1024 * 1024; // 1MiB

extern "C" {
    static _app_start_addr: usize;
    static _app_size: usize;
}

pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError> {
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + HEAP_SIZE - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        mapper
            .map_to(page, frame, flags, PageTableFlags::empty(), frame_allocator)?
            .flush();
    }

    unsafe {
        crate::ALLOCATOR.lock().init(HEAP_START, HEAP_SIZE);
    }

    Ok(())
}

pub fn init_stack(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError> {
    let stack_start = VirtAddr::new(STACK_START as u64);
    let stack_end = stack_start + STACK_SIZE - 1u64;
    let stack_start_page = Page::containing_address(stack_start);
    let stack_end_page = Page::containing_address(stack_end);

    let page_range = { Page::range_inclusive(stack_start_page + 1, stack_end_page) };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        mapper
            .map_to(page, frame, flags, PageTableFlags::empty(), frame_allocator)?
            .flush();
    }

    // Guard Page
    let frame = frame_allocator
        .allocate_frame()
        .ok_or(MapToError::FrameAllocationFailed)?;
    let flags = PageTableFlags::PRESENT;
    mapper
        .map_to(
            stack_start_page,
            frame,
            flags,
            PageTableFlags::empty(),
            frame_allocator,
        )?
        .flush();

    unsafe {
        println!("load_tss");
        use x86_64::instructions::tables::load_tss;
        gdt::GDT.as_ref().unwrap().0.load();
        gdt::TSS.as_mut().unwrap().privilege_stack_table[0] = stack_end;
        //println!("privilege_stack_table[0] = 0x{:X}", stack_end.as_u64());
        load_tss(gdt::GDT.as_ref().unwrap().1.tss_selector);
    }

    Ok(())
}

pub struct Dummy;

unsafe impl GlobalAlloc for Dummy {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        panic!("dealloc should be never called")
    }
}

static mut ENTRY_POINT: Option<
    fn(mapper: &mut OffsetPageTable, frame_allocator: &mut BootInfoFrameAllocator) -> !,
> = None;
static mut FRAME_ALLOCATOR: Option<BootInfoFrameAllocator> = None;
static mut MAPPER: Option<OffsetPageTable> = None;

pub unsafe fn init_offset_pagetable() {
    let p3o: &mut [u64] = core::slice::from_raw_parts_mut(PDPTE_OFFSET_START as _, 512);
    for i in 0..512 {
        p3o[i] = ((i as u64) << 30) | 0x183u64;
    }
    let (level_4_table_frame, _) = Cr3::read();

    let pml4t: &mut [u64] =
        core::slice::from_raw_parts_mut(level_4_table_frame.start_address().as_u64() as _, 512);

    // Entry covering VA [0..512GB) with physical offset PHYSICAL_MEMORY_OFFSET
    pml4t[(PHYSICAL_MEMORY_OFFSET >> 39) as usize & 0x1FFusize] = PDPTE_OFFSET_START as u64 | 0x7;

    x86_64::instructions::tlb::flush(VirtAddr::new(level_4_table_frame.start_address().as_u64()));
    x86_64::instructions::tlb::flush(VirtAddr::new(PDPTE_OFFSET_START as _));
}

pub fn init(
    boot_info: &'static mut BootInfo,
    entry_point: fn(
        mapper: &mut OffsetPageTable,
        frame_allocator: &mut BootInfoFrameAllocator,
    ) -> !,
) -> ! {
    unsafe {
        init_offset_pagetable();
    }
    gdt::init();
    unsafe { syscall::init() };
    interrupts::init();

    println!("{:#?}", boot_info);

    let phys_mem_offset = VirtAddr::new(boot_info.physical_memory_offset);

    unsafe { MAPPER.replace(crate::memory::init(phys_mem_offset)) };

    let mut frame_allocator = unsafe { BootInfoFrameAllocator::init(&mut boot_info.memory_map) };

    init_heap(unsafe { MAPPER.as_mut().unwrap() }, &mut frame_allocator)
        .expect("heap initialization failed");

    init_stack(unsafe { MAPPER.as_mut().unwrap() }, &mut frame_allocator)
        .expect("stack initialization failed");

    unsafe {
        FRAME_ALLOCATOR.replace(frame_allocator);
        ENTRY_POINT.replace(entry_point);
    }

    unsafe { crate::context_switch(init_after_stack_swap, STACK_START + STACK_SIZE) }
}

fn init_after_stack_swap() -> ! {
    let frame_allocator = unsafe { FRAME_ALLOCATOR.as_mut().unwrap() };
    let mapper = unsafe { MAPPER.as_mut().unwrap() };
    let entry_point = unsafe { ENTRY_POINT.take().unwrap() };

    frame_allocator.set_region_type_usable(MemoryRegionType::KernelStack);

    entry_point(mapper, frame_allocator)
}

// TODO: muti-thread or syscall-proxy
pub static mut NEXT_MMAP: u64 = 0;

// TODO: muti-thread or syscall-proxy
pub fn mmap_user(len: usize) -> *mut u8 {
    let virt_start_addr;
    unsafe {
        virt_start_addr = VirtAddr::new(NEXT_MMAP as u64);
    }
    let start_page: Page = Page::containing_address(virt_start_addr);
    let end_page: Page = Page::containing_address(virt_start_addr + len - 1u64);
    let page_range = Page::range_inclusive(start_page, end_page);

    let mut frame_allocator;
    let mut mapper;
    unsafe {
        frame_allocator = FRAME_ALLOCATOR.take().unwrap();
        mapper = MAPPER.take().unwrap();
    }
    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)
            .unwrap();
        //println!("page {:#?} frame {:#?}", page, frame);
        mapper
            .map_to(
                page,
                frame,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::USER_ACCESSIBLE,
                PageTableFlags::USER_ACCESSIBLE,
                &mut frame_allocator,
            )
            .and_then(|f| {
                f.flush();
                Ok(())
            })
            .or_else(|e| match e {
                MapToError::PageAlreadyMapped => Ok(()),
                _ => Err(e),
            })
            .unwrap();
    }

    let ret;
    unsafe {
        ret = NEXT_MMAP as *mut u8;
        NEXT_MMAP += len as u64;
        FRAME_ALLOCATOR.replace(frame_allocator);
        MAPPER.replace(mapper);
    }
    ret
}

#[derive(Debug)]
pub struct Memory {
    start: VirtAddr,
    size: usize,
    flags: PageTableFlags,
}

#[derive(Debug)]
pub struct Tls {
    pub master: VirtAddr,
    pub file_size: usize,
    pub mem: Memory,
    pub offset: usize,
}

impl Tls {
    /*
    /// Load TLS data from master
    pub unsafe fn load(&mut self) {
        core::mem::intrinsics::copy(
            self.master.get() as *const u8,
            (self.mem.start_address().get() + self.offset) as *mut u8,
            self.file_size,
        );
    }
    */
}

pub fn exec_app(mapper: &mut OffsetPageTable, frame_allocator: &mut BootInfoFrameAllocator) -> ! {
    use xmas_elf::program::ProgramHeader;

    let virt_start_addr = VirtAddr::new(USER_STACK_OFFSET as u64);
    let start_page: Page = Page::containing_address(virt_start_addr);
    let end_page: Page = Page::containing_address(virt_start_addr + USER_STACK_SIZE - 256u64);
    let page_range = Page::range_inclusive(start_page, end_page);

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)
            .unwrap();
        mapper
            .map_to(
                page,
                frame,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::USER_ACCESSIBLE,
                PageTableFlags::USER_ACCESSIBLE,
                frame_allocator,
            )
            .unwrap()
            .flush();
    }

    // Extract required information from the ELF file.
    let entry_point;
    let app_start_ptr = unsafe { &_app_start_addr as *const _ as u64 };
    unsafe {
        println!("app start {:#X}", app_start_ptr);
        println!("app size {:#X}", &_app_size as *const _ as u64);
    }
    let kernel = unsafe {
        core::slice::from_raw_parts(
            &_app_start_addr as *const _ as *const u8,
            &_app_size as *const _ as usize,
        )
    };
    let elf_file = xmas_elf::ElfFile::new(kernel).unwrap();
    xmas_elf::header::sanity_check(&elf_file).unwrap();

    entry_point = elf_file.header.pt2.entry_point();

    //let mut user_tls = false;

    for program_header in elf_file.program_iter() {
        match program_header {
            ProgramHeader::Ph64(header) => {
                let segment = *header;
                //println!("{:#?}", segment);
                let _has_tls = map_user_segment(
                    &segment,
                    PhysAddr::new(app_start_ptr),
                    mapper,
                    frame_allocator,
                )
                .unwrap();
                /*
                if has_tls == true {
                    user_tls = true;
                }
                */
            }
            ProgramHeader::Ph32(_) => panic!("does not support 32 bit elf files"),
        }
    }

    println!("app_entry_point={:#X}", entry_point);
    println!("{}:{}", file!(), line!());

    use crate::alloc::string::ToString;
    let mut crt0sp = crt0stack::Crt0Stack::new();
    crt0sp.argv.push("/init".to_string());
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_EGID,
        value: 1,
    });
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_GID,
        value: 1,
    });
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_UID,
        value: 1,
    });
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_EUID,
        value: 1,
    });
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_PAGESZ,
        value: 4096,
    });
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_SECURE,
        value: 0,
    });
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_CLKTCK,
        value: 100,
    });
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_FLAGS,
        value: 0,
    });

    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_HWCAP,
        value: 0xbfebfbff,
    });
    crt0sp.auxv.push(crt0stack::AuxvPair {
        key: crt0stack::AT_HWCAP2,
        value: 1, // HWCAP2_FSGSBASE
    });
    let r1 = x86_64::instructions::random::RdRand::new()
        .unwrap()
        .get_u64()
        .unwrap();
    let r2 = x86_64::instructions::random::RdRand::new()
        .unwrap()
        .get_u64()
        .unwrap();

    crt0sp.random = Some([0u8; 16]);

    {
        let ra = &mut crt0sp.random.unwrap();
        let r1u8 = unsafe { core::slice::from_raw_parts(&r1 as *const u64 as *const u8, 8) };
        let r2u8 = unsafe { core::slice::from_raw_parts(&r2 as *const u64 as *const u8, 8) };
        ra[0..8].copy_from_slice(r1u8);
        ra[8..16].copy_from_slice(r2u8);
    }

    crt0sp.platform = Some("x86_64".to_string());
    crt0sp.exec_fn = Some("/init".to_string());

    let sp_slice =
        unsafe { core::slice::from_raw_parts_mut((USER_STACK_OFFSET) as *mut u8, USER_STACK_SIZE) };

    let sp_idx = crt0sp.serialize(sp_slice);
    let sp = &mut sp_slice[sp_idx] as *mut u8 as usize;
    println!("stackpointer={:#X}", sp);
    println!("USER_STACK_OFFSET={:#X}", USER_STACK_OFFSET);

    unsafe {
        syscall::usermode(entry_point as usize, sp, 0);
    }
}

pub(crate) fn map_user_segment(
    segment: &ProgramHeader64,
    file_start: PhysAddr,
    page_table: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<Option<Tls>, MapToError> {
    let typ = segment.get_type().unwrap();
    let mut tls_ret: Option<Tls> = None;

    match typ {
        program::Type::Load => {
            let mem_size = segment.mem_size;
            let file_size = segment.file_size;
            let file_offset = segment.offset;
            let phys_start_addr = file_start + file_offset;
            let virt_start_addr = VirtAddr::new(segment.virtual_addr);
            let virt_end_addr = (virt_start_addr + segment.mem_size as u64).align_up(4096u64);

            unsafe {
                if NEXT_MMAP < virt_end_addr.as_u64() {
                    NEXT_MMAP = virt_end_addr.as_u64();
                    //println!("NEXT_MMAP = {:X}", NEXT_MMAP);
                }
            }

            let start_page: Page = Page::containing_address(virt_start_addr);
            let end_page: Page = Page::containing_address(virt_start_addr + mem_size - 1u64);
            let page_range = Page::range_inclusive(start_page, end_page);
            //println!("{:#?}", page_range);

            let flags = segment.flags;
            let mut page_table_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if !flags.is_execute() {
                page_table_flags |= PageTableFlags::NO_EXECUTE
            };
            if flags.is_write() {
                page_table_flags |= PageTableFlags::WRITABLE
            };

            for page in page_range {
                let frame = frame_allocator
                    .allocate_frame()
                    .ok_or(MapToError::FrameAllocationFailed)?;
                page_table
                    .map_to(
                        page,
                        frame,
                        page_table_flags | PageTableFlags::WRITABLE,
                        PageTableFlags::USER_ACCESSIBLE,
                        frame_allocator,
                    )
                    .and_then(|f| {
                        f.flush();
                        Ok(())
                    })
                    .or_else(|e| match e {
                        MapToError::PageAlreadyMapped => Ok(()),
                        _ => Err(e),
                    })?;
            }
            unsafe {
                let src = core::slice::from_raw_parts(
                    phys_start_addr.as_u64() as *const u8,
                    file_size as _,
                );
                let dst = core::slice::from_raw_parts_mut(
                    virt_start_addr.as_mut_ptr::<u8>(),
                    file_size as _,
                );
                dst.copy_from_slice(src);

                let dst = core::slice::from_raw_parts_mut(
                    (virt_start_addr + file_size).as_mut_ptr::<u8>(),
                    mem_size as usize - file_size as usize,
                );
                dst.iter_mut().for_each(|i| *i = 0);
            }
            for page in page_range {
                page_table
                    .update_flags(page, page_table_flags)
                    .unwrap()
                    .flush();
            }
        }
        /*
        program::Type::Tls => {
            let aligned_size = if segment.align > 0 {
                ((segment.mem_size + (segment.align - 1)) / segment.align) * segment.align
            } else {
                segment.mem_size
            } as usize;
            let rounded_size = ((aligned_size + PAGE_SIZE - 1) / PAGE_SIZE) * PAGE_SIZE;
            let rounded_offset = rounded_size - aligned_size;

            // TODO: Make sure size is not greater than USER_TLS_SIZE
            let tls_addr = USER_TLS_OFFSET /*+ context.id.into() * crate::USER_TLS_SIZE */;
            let tls = Tls {
                master: VirtAddr::new(segment.virtual_addr),
                file_size: segment.file_size as usize,
                mem: Memory::new(
                    VirtAddr::new(tls_addr as u64),
                    rounded_size as usize,
                    PageTableFlags::NO_EXECUTE
                        | PageTableFlags::WRITABLE
                        | PageTableFlags::USER_ACCESSIBLE,
                    true,
                ),
                offset: rounded_offset as usize,
            };

            unsafe {
                *(tcb_addr as *mut usize) = tls.mem.start_address().get() + tls.mem.size();
            }

            tls_ret = Some(tls);
        }
        */
        _ => {}
    }
    Ok(tls_ret)
}
