use crate::arch::x86_64::{
    consts::*,
    gdt::{gdt_entry, kvm_segment_from_gdt},
    structures::paging::{frame::PhysFrameRange, PhysFrame},
    HostVirtAddr, PhysAddr, VirtAddr,
};
use crate::error::*;
use crate::{context, map_context};
use kvm_bindings::{
    kvm_mp_state, kvm_pit_config, kvm_segment, kvm_userspace_memory_region, KVM_MAX_CPUID_ENTRIES,
    KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use linux_errno::ErrNo;
use std::io::Write;
use vmsyscall::bootinfo::BootInfo;
use vmsyscall::memory_map::{FrameRange, MemoryMap, MemoryRegion, MemoryRegionType};
use vmsyscall::{VmSyscall, VmSyscallRet};

const DEFAULT_GUEST_MEM: u64 = 2 * 1024 * 1024 * 1024; // 2GiB
const DEFAULT_GUEST_PAGE_SIZE: usize = 4096;

pub const HIMEM_START: usize = 0x0010_0000; //1 MB.

pub const SYSCALL_PHYS_ADDR: u64 = 0x1000;
pub const SYSCALL_TRIGGER_PORT: u16 = 0xFF;

// Initial pagetables.
pub const PML4_START: usize = 0x9000;
pub const PDPTE_START: usize = 0xA000;
pub const PDE_START: usize = 0xB000;
pub const PAGETABLE_LEN: u64 = core::mem::size_of::<PageTables>() as _;

pub const BOOT_GDT_OFFSET: usize = 0x500;
pub const BOOT_IDT_OFFSET: usize = 0x520;

#[repr(C)]
pub struct PageTables {
    pub pml4t: [u64; 512],
    pub pml3t_ident: [u64; 512],
    pub pml2t_ident: [u64; 512],
}

impl Default for PageTables {
    fn default() -> Self {
        PageTables {
            pml4t: [0u64; 512],
            pml3t_ident: [0u64; 512],
            pml2t_ident: [0u64; 512],
        }
    }
}

struct UserspaceMemRegion {
    region: kvm_userspace_memory_region,
    host_mem: HostVirtAddr,
    mmap_start: HostVirtAddr,
    mmap_size: usize,
}

pub struct KvmVm {
    pub kvm: Kvm,
    pub cpu_fd: Vec<VcpuFd>,
    pub kvm_fd: VmFd,
    page_size: usize,
    memory_map: MemoryMap,
    userspace_mem_regions: Vec<UserspaceMemRegion>,
    has_irqchip: bool,
    pub syscall_hostvaddr: Option<HostVirtAddr>,
}

fn frame_range(range: PhysFrameRange) -> FrameRange {
    FrameRange::new(
        range.start.start_address().as_u64(),
        range.end.start_address().as_u64(),
    )
}

impl KvmVm {
    pub fn vm_create(phy_pages: u64) -> Result<Self, Error> {
        let kvm = Kvm::new().unwrap();

        let kvm_fd: VmFd = kvm.create_vm().map_err(|e| ErrorKind::from(&e))?;

        let mut vm = KvmVm {
            kvm,
            cpu_fd: vec![],
            kvm_fd,
            page_size: DEFAULT_GUEST_PAGE_SIZE,
            memory_map: MemoryMap::new(),
            userspace_mem_regions: vec![],
            has_irqchip: false,
            syscall_hostvaddr: None,
        };

        //FIXME: remove phy_pages
        if phy_pages != 0 {
            vm.vm_userspace_mem_region_add(PhysAddr::new(0), 0, phy_pages, 0)?;

            let zero_frame: PhysFrame = PhysFrame::from_start_address(PhysAddr::new(0)).unwrap();

            vm.memory_map.mark_allocated_region(MemoryRegion {
                range: frame_range(PhysFrame::range(zero_frame, zero_frame + 1)),
                region_type: MemoryRegionType::FrameZero,
            });

            let syscall_frame: PhysFrame =
                PhysFrame::from_start_address(PhysAddr::new(SYSCALL_PHYS_ADDR)).unwrap();
            vm.memory_map.mark_allocated_region(MemoryRegion {
                range: frame_range(PhysFrame::range(syscall_frame, syscall_frame + 1)),
                region_type: MemoryRegionType::InUse,
            });

            vm.setup_page_tables()?;
        }

        Ok(vm)
    }

    pub fn vm_userspace_mem_region_add(
        &mut self,
        guest_paddr: PhysAddr,
        slot: u32,
        npages: u64,
        flags: u32,
    ) -> Result<(), Error> {
        for r in self.userspace_mem_regions.iter() {
            if r.region.slot == slot {
                return Err(context!(ErrorKind::MemRegionWithSlotAlreadyExists));
            }

            if guest_paddr.as_u64() <= (r.region.guest_phys_addr + r.region.memory_size)
                && (guest_paddr.as_u64() + npages * self.page_size as u64)
                    >= r.region.guest_phys_addr
            {
                return Err(context!(ErrorKind::OverlappingUserspaceMemRegionExists));
            }
        }

        let mut region = UserspaceMemRegion {
            region: Default::default(),
            host_mem: HostVirtAddr::new(0),
            mmap_start: HostVirtAddr::new(0),
            mmap_size: (npages * self.page_size as u64) as _,
        };
        let mm = mmap::MemoryMap::new(
            region.mmap_size,
            &[mmap::MapOption::MapReadable, mmap::MapOption::MapWritable],
        )
        .map_err(|_| context!(ErrorKind::MmapFailed))?;
        let mmap_start = mm.data();
        // FIXME: No drop for mm
        std::mem::forget(mm);

        region.mmap_start = HostVirtAddr::new(mmap_start as u64);

        region.host_mem = region.mmap_start;

        region.region.slot = slot;
        region.region.flags = flags;
        region.region.guest_phys_addr = guest_paddr.as_u64();
        region.region.memory_size = npages * self.page_size as u64;
        region.region.userspace_addr = region.host_mem.as_u64();

        unsafe {
            self.kvm_fd
                .set_user_memory_region(region.region)
                .map_err(|e| ErrorKind::from(&e))?
        };

        self.memory_map.add_region(MemoryRegion {
            range: FrameRange::new(
                region.region.guest_phys_addr,
                region.region.guest_phys_addr + region.region.memory_size,
            ),
            region_type: MemoryRegionType::Usable,
        });

        self.userspace_mem_regions.push(region);

        Ok(())
    }

    pub fn addr_gpa2hva(&self, guest_phys_addr: PhysAddr) -> Result<HostVirtAddr, Error> {
        for region in &self.userspace_mem_regions {
            if (guest_phys_addr.as_u64() >= region.region.guest_phys_addr)
                && (guest_phys_addr.as_u64()
                    <= (region.region.guest_phys_addr + region.region.memory_size - 1))
            {
                return Ok(HostVirtAddr::new(
                    region.host_mem.as_u64()
                        + (guest_phys_addr.as_u64() - region.region.guest_phys_addr),
                ));
            }
        }
        Err(context!(ErrorKind::NoMappingForVirtualAddress))
    }

    fn setup_page_tables(&mut self) -> Result<(), Error> {
        let mut page_tables = PageTables::default();

        // Note we are assuming CPU supports 2MB pages. All modern CPUs do.
        page_tables.pml4t[0] = PDPTE_START as u64 | 0x7;
        page_tables.pml3t_ident[0] = PDE_START as u64 | 0x7;
        page_tables.pml2t_ident[0] = 0x183u64;

        let guest_pg_addr: *mut PageTables = self
            .addr_gpa2hva(PhysAddr::new(PML4_START as _))?
            .as_mut_ptr();

        unsafe {
            // FIXME: SEV LOAD
            guest_pg_addr.write(page_tables);
        }

        Ok(())
    }

    pub fn elf_load(
        &mut self,
        program_invocation_name: &str,
        region_type: MemoryRegionType,
    ) -> Result<(VirtAddr, VirtAddr, usize), Error> {
        use std::fs::File;
        use std::os::unix::io::AsRawFd;
        use xmas_elf::program::{self, ProgramHeader};
        use xmas_elf::ElfFile;

        let file = File::open(program_invocation_name).map_err(map_context!())?;
        let mmap_size = file.metadata().map_err(map_context!())?.len() as usize;
        let mm = mmap::MemoryMap::new(
            mmap_size,
            &[
                mmap::MapOption::MapFd(file.as_raw_fd()),
                mmap::MapOption::MapReadable,
            ],
        )
        .map_err(|_| context!(ErrorKind::MmapFailed))?;

        let data = unsafe { core::slice::from_raw_parts(mm.data(), mmap_size) };

        let elf_file = ElfFile::new(data).map_err(map_context!())?;

        xmas_elf::header::sanity_check(&elf_file).map_err(map_context!())?;

        let guest_code: VirtAddr = VirtAddr::new(elf_file.header.pt2.entry_point());
        let mut load_addr: Option<VirtAddr> = None;
        let phnum: usize = elf_file.program_iter().count();

        for program_header in elf_file.program_iter() {
            match program_header {
                ProgramHeader::Ph64(header) => {
                    let segment = *header;
                    match segment.get_type().unwrap() {
                        program::Type::Load => {}
                        program::Type::Interp => {
                            return Err(ErrorKind::NotAStaticBinary.into());
                        }
                        _ => continue,
                    }

                    if load_addr.is_none() {
                        load_addr.replace(VirtAddr::new(segment.virtual_addr) - segment.offset);
                    }

                    // dbg!(segment);

                    if segment.mem_size == 0 {
                        continue;
                    }

                    let start_phys = PhysAddr::new(segment.physical_addr);
                    let start_frame: PhysFrame =
                        PhysFrame::from_start_address(start_phys.align_down(self.page_size as u64))
                            .unwrap();

                    let end_frame: PhysFrame = PhysFrame::from_start_address(
                        PhysAddr::new((segment.physical_addr) + segment.mem_size - 1)
                            .align_up(self.page_size as u64),
                    )
                    .unwrap();

                    let region = MemoryRegion {
                        range: frame_range(PhysFrame::range(start_frame, end_frame)),
                        region_type,
                    };

                    //dbg!(region);
                    //dbg!(&self.memory_map);
                    self.memory_map.mark_allocated_region(region);

                    // FIXME: SEV LOAD
                    let host_slice = unsafe {
                        core::slice::from_raw_parts_mut(
                            self.addr_gpa2hva(start_phys)?.as_u64() as *mut u8,
                            segment.mem_size as usize,
                        )
                    };

                    host_slice[..segment.file_size as usize].copy_from_slice(
                        &data[segment.offset as usize
                            ..(segment.offset + segment.file_size) as usize],
                    );

                    unsafe {
                        if segment.mem_size > segment.file_size {
                            core::ptr::write_bytes(
                                &mut host_slice[segment.file_size as usize] as *mut u8,
                                0u8,
                                segment.mem_size as usize - segment.file_size as usize,
                            );
                        }
                    }
                }
                ProgramHeader::Ph32(_) => panic!("does not support 32 bit elf files"),
            }
        }

        Ok((guest_code, load_addr.unwrap(), phnum))
    }

    fn write_gdt_table(&self, table: &[u64]) -> Result<(), Error> {
        let gdt_addr: *mut u64 = self
            .addr_gpa2hva(PhysAddr::new(BOOT_GDT_OFFSET as _))?
            .as_mut_ptr();
        for (index, entry) in table.iter().enumerate() {
            let addr = unsafe { gdt_addr.offset(index as _) };
            unsafe { addr.write(*entry) };
        }
        Ok(())
    }

    fn write_idt_value(&self, val: u64) -> Result<(), Error> {
        let boot_idt_addr: *mut u64 = self
            .addr_gpa2hva(PhysAddr::new(BOOT_IDT_OFFSET as _))?
            .as_mut_ptr();
        unsafe { boot_idt_addr.write(val) }
        Ok(())
    }

    pub fn vcpu_setup(&mut self, vcpuid: u8) -> Result<(), Error> {
        let mut sregs = self.cpu_fd[vcpuid as usize]
            .get_sregs()
            .map_err(|e| ErrorKind::from(&e))?;

        let gdt_table: [u64; 4] = [
            gdt_entry(0, 0, 0),            // NULL
            gdt_entry(0xa09b, 0, 0xfffff), // CODE
            gdt_entry(0xc093, 0, 0xfffff), // DATA
            gdt_entry(0x808b, 0, 0xfffff), // TSS
        ];

        let null_seg = kvm_segment_from_gdt(gdt_table[0], 0);
        let code_seg = kvm_segment_from_gdt(gdt_table[1], 1);
        let data_seg = kvm_segment_from_gdt(gdt_table[2], 2);
        let tss_seg = kvm_segment_from_gdt(gdt_table[3], 3);

        // Write segments
        self.write_gdt_table(&gdt_table[..])?;
        sregs.gdt.base = BOOT_GDT_OFFSET as u64;
        sregs.gdt.limit = core::mem::size_of_val(&gdt_table) as u16 - 1;

        self.write_idt_value(0)?;
        sregs.idt.base = BOOT_IDT_OFFSET as u64;
        sregs.idt.limit = core::mem::size_of::<u64>() as u16 - 1;

        // kvm_seg_set_unusable(&mut sregs.ldt);
        sregs.ldt = kvm_segment {
            base: 0,
            limit: 0,
            selector: 0,
            type_: 0,
            present: 0,
            dpl: 0,
            db: 0,
            s: 0,
            l: 0,
            g: 0,
            avl: 0,
            padding: 0,
            unusable: 1,
        };

        sregs.cs = code_seg;
        sregs.ss = data_seg;

        sregs.ds = null_seg;
        sregs.es = null_seg;
        sregs.fs = null_seg;
        sregs.gs = null_seg;

        sregs.tr = tss_seg;

        sregs.cr0 = (X86_CR0_PE | X86_CR0_NE | X86_CR0_PG | X86_CR0_ET | X86_CR0_MP) as u64;
        //sregs.cr0 &= !(X86_CR0_EM as u64);
        sregs.cr4 = (X86_CR4_PAE | X86_CR4_OSFXSR | X86_CR4_OSXMMEXCPT) as u64;
        sregs.efer = (EFER_LME | EFER_LMA) as u64;

        sregs.cr3 = PML4_START as _;

        /*
        sregs.cr8 = 0;
        sregs.apic_base = 1 << 11;
        */

        self.cpu_fd[vcpuid as usize]
            .set_sregs(&sregs)
            .map_err(|e| ErrorKind::from(&e))?;

        Ok(())
    }

    fn vcpu_add(&mut self, vcpuid: u8) -> Result<(), Error> {
        let vcpu_fd = self
            .kvm_fd
            .create_vcpu(vcpuid)
            .map_err(|e| ErrorKind::from(&e))?;
        self.cpu_fd.insert(vcpuid as usize, vcpu_fd);
        self.vcpu_setup(vcpuid)?;

        Ok(())
    }

    fn vcpu_add_default(
        &mut self,
        vcpuid: u8,
        guest_code: VirtAddr,
        elf_code: VirtAddr,
        elf_phdr: VirtAddr,
        elf_phnum: usize,
    ) -> Result<(), Error> {
        let syscall_vaddr = PhysAddr::new(SYSCALL_PHYS_ADDR);

        self.syscall_hostvaddr = Some(self.addr_gpa2hva(syscall_vaddr)?);

        let mut boot_info = BootInfo {
            memory_map: self.memory_map.clone(),
            entry_point: elf_code.as_ptr(),
            load_addr: elf_phdr.as_ptr(),
            elf_phnum: elf_phnum,
            syscall_trigger_port: SYSCALL_TRIGGER_PORT,
        };

        boot_info.memory_map.sort();
        // Write boot info to syscall page.
        unsafe {
            self.syscall_hostvaddr
                .unwrap()
                .as_mut_ptr::<BootInfo>()
                .write(boot_info)
        };

        /* Create VCPU */
        self.vcpu_add(vcpuid)?;

        /* Setup guest general purpose registers */
        let mut regs = self.cpu_fd[vcpuid as usize]
            .get_regs()
            .map_err(|e| ErrorKind::from(&e))?;
        regs.rflags |= 0x2;
        regs.rip = guest_code.as_u64();
        regs.rdi = syscall_vaddr.as_u64();

        self.cpu_fd[vcpuid as usize]
            .set_regs(&regs)
            .map_err(|e| ErrorKind::from(&e))?;

        /* Setup the MP state */
        let mp_state: kvm_mp_state = kvm_mp_state { mp_state: 0 };
        self.cpu_fd[vcpuid as usize]
            .set_mp_state(mp_state)
            .map_err(|e| ErrorKind::from(&e))?;

        Ok(())
    }

    pub fn handle_syscall(&mut self) -> Result<(), ()> {
        unsafe {
            let syscall_page = self.syscall_hostvaddr.unwrap();
            let request: *mut VmSyscall = syscall_page.as_mut_ptr();
            let reply: *mut VmSyscallRet = syscall_page.as_mut_ptr();

            //eprintln!("vmsyscall in: {:#?}", &*request);

            reply.write_volatile(match request.read_volatile() {
                VmSyscall::Write { fd, count, data } => match fd {
                    1 => {
                        let mut count: usize = count;
                        if count > 4000 {
                            count = 4000;
                        }
                        VmSyscallRet::Write(
                            std::io::stdout()
                                .write_all(&data[..count])
                                .map(|_| count as _)
                                .map_err(|e| {
                                    vmsyscall::Error::Errno(
                                        e.raw_os_error()
                                            .unwrap_or(Into::<i64>::into(ErrNo::EBADF) as _)
                                            .into(),
                                    )
                                }),
                        )
                    }
                    2 => {
                        let mut count: usize = count;
                        if count > 4000 {
                            count = 4000;
                        }
                        VmSyscallRet::Write(
                            std::io::stderr()
                                .write_all(&data[..count])
                                .map(|_| count as _)
                                .map_err(|e| {
                                    vmsyscall::Error::Errno(
                                        e.raw_os_error()
                                            .unwrap_or(Into::<i64>::into(ErrNo::EBADF) as _)
                                            .into(),
                                    )
                                }),
                        )
                    }
                    _ => VmSyscallRet::Write(Err(vmsyscall::Error::Errno(ErrNo::EBADF.into()))),
                },
                VmSyscall::Read { fd: _, count: _ } => {
                    VmSyscallRet::Read(Err(vmsyscall::Error::Errno(ErrNo::EBADF.into())))
                }
                VmSyscall::Mmap {
                    addr: _,
                    length: _,
                    prot: _,
                    flags: _,
                } => {
                    VmSyscallRet::Mmap(Err(vmsyscall::Error::Errno(ErrNo::ENOSYS.into())))
                    /*
                    let ret = unsafe {
                        mmap(
                            null_mut(),
                            len,
                            ProtFlags::from_bits_truncate(prot),
                            MapFlags::from_bits_truncate(flags),
                            -1,
                            0,
                        )
                    };
                    let mmap_start = match ret {
                        Err(nix::Error::Sys(e)) if e == nix::errno::Errno::ENOMEM => {
                            return KvmSyscallRet::Mmap(Err(vmsyscall::Error::ENOMEM))
                        }
                        Err(_) => return KvmSyscallRet::Mmap(Err(vmsyscall::Error::OTHERERROR)),
                        Ok(v) => v,
                    };
                    let mut region = UserspaceMemRegion {
                        region: Default::default(),
                        used_phy_pages: Default::default(),
                        host_mem: PhysAddr::new(mmap_start as u64),
                        mmap_start: PhysAddr::new(mmap_start as u64),
                        mmap_size: len as _,
                    };

                    region.region.slot = 0;
                    region.region.flags = flags as _;
                    region.region.guest_phys_addr = addr as _;
                    region.region.memory_size = len as _;
                    region.region.userspace_addr = region.host_mem.as_u64();

                    unsafe {
                        self.kvm_fd
                            .set_user_memory_region(region.region)
                            .map_err(map_context!())?
                    };

                    //self.userspace_mem_regions.push(region);

                    KvmSyscallRet::Mmap(Ok(region.mmap_start.as_u64() as _))
                    */
                }
                VmSyscall::Madvise {
                    addr: _,
                    length: _,
                    advice: _,
                } => VmSyscallRet::Madvise(Err(vmsyscall::Error::Errno(ErrNo::ENOSYS.into()))),
                VmSyscall::Mremap {
                    old_address: _,
                    old_size: _,
                    new_size: _,
                    flags: _,
                } => VmSyscallRet::Mremap(Err(vmsyscall::Error::Errno(ErrNo::ENOSYS.into()))),
                VmSyscall::Munmap { addr: _, length: _ } => {
                    VmSyscallRet::Munmap(Err(vmsyscall::Error::Errno(ErrNo::ENOSYS.into())))
                }
                VmSyscall::Mprotect {
                    addr: _,
                    length: _,
                    prot: _,
                } => VmSyscallRet::Mprotect(Err(vmsyscall::Error::Errno(ErrNo::ENOSYS.into()))),
            });
        }
        Ok(())
    }

    fn create_irqchip(&mut self) -> Result<(), Error> {
        self.kvm_fd
            .create_irq_chip()
            .map_err(|e| ErrorKind::from(&e))?;
        self.has_irqchip = true;

        let mut pit_config = kvm_pit_config::default();
        // We need to enable the emulation of a dummy speaker port stub so that writing to port 0x61
        // (i.e. KVM_SPEAKER_BASE_ADDRESS) does not trigger an exit to user space.
        pit_config.flags = KVM_PIT_SPEAKER_DUMMY;
        self.kvm_fd
            .create_pit2(pit_config)
            .map_err(|e| ErrorKind::from(&e))?;

        Ok(())
    }

    pub fn vm_create_default(kernel_name: &str, elf_name: &str, vcpuid: u8) -> Result<Self, Error> {
        /* Create VM */
        let mut vm = KvmVm::vm_create((DEFAULT_GUEST_MEM / DEFAULT_GUEST_PAGE_SIZE as u64) as _)?;

        /* Setup IRQ Chip */
        vm.create_irqchip()?;

        /* Setup app guest code */
        let (elf_code, elf_phdr, elf_phnum) = vm.elf_load(elf_name, MemoryRegionType::App)?;

        /* Setup kernel guest code */
        let (guest_code, _, _) = vm.elf_load(kernel_name, MemoryRegionType::Kernel)?;

        /* Add the first vCPU. */
        vm.vcpu_add_default(vcpuid, guest_code, elf_code, elf_phdr, elf_phnum)?;

        /* Set CPUID */
        let cpuid = vm
            .kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(|e| ErrorKind::from(&e))?;

        vm.cpu_fd[vcpuid as usize]
            .set_cpuid2(&cpuid)
            .map_err(|e| ErrorKind::from(&e))?;

        Ok(vm)
    }
}
