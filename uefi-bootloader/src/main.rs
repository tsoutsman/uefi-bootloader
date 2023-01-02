#![allow(dead_code)]
#![feature(step_trait, abi_efiapi, maybe_uninit_slice, maybe_uninit_write_slice)]
#![no_std]
#![no_main]

mod arch;
mod kernel;
mod logger;
mod memory;
mod modules;
mod util;

use crate::arch::context_switch;
use crate::memory::{
    set_up_arch_specific_mappings, Frame, Memory, Page, PhysicalAddress, PteFlags, VirtualAddress,
};
use core::{alloc::Layout, fmt::Write, iter::Peekable, mem::MaybeUninit, ptr::NonNull, slice};
use log::{error, info};
use uefi::{
    prelude::entry,
    proto::console::gop::{self, GraphicsOutput},
    table::{
        boot::{AllocateType, MemoryDescriptor, MemoryType},
        cfg::{ACPI2_GUID, ACPI_GUID},
        Boot, SystemTable,
    },
    Handle, Status,
};
use uefi_bootloader_api::{
    BootInformation, ElfSection, ElfSections, FrameBuffer, FrameBufferInfo, MemoryRegion,
    MemoryRegionKind, MemoryRegions, Module, Modules, PixelFormat,
};

static mut SYSTEM_TABLE: Option<NonNull<SystemTable<Boot>>> = None;

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    let system_table_pointer = NonNull::from(&mut system_table);
    unsafe { SYSTEM_TABLE = Some(system_table_pointer) };

    system_table
        .stdout()
        .clear()
        .expect("failed to clear stdout");

    let frame_buffer = get_frame_buffer(&system_table);
    if let Some(frame_buffer) = frame_buffer {
        init_logger(&frame_buffer);
        info!("using framebuffer at {:#x}", frame_buffer.start);
    }

    unsafe { SYSTEM_TABLE = None };

    let rsdp_address = get_rsdp_address(&system_table);

    let mut memory = Memory::new(system_table.boot_services());

    let (entry_point, elf_sections) = kernel::load(handle, &system_table, &mut memory);
    info!("loaded kernel");
    let modules = modules::load(handle, &system_table);

    let mappings = set_up_mappings(&mut memory, &frame_buffer);
    info!("created memory mappings");

    let page_table = memory.page_table();

    let memory_map_size = system_table.boot_services().memory_map_size().map_size
        + 8 * core::mem::size_of::<MemoryDescriptor>();

    let memory_map_storage = {
        let pointer = system_table
            .boot_services()
            .allocate_pages(
                AllocateType::AnyPages,
                MemoryType::LOADER_DATA,
                util::calculate_pages(memory_map_size),
            )
            .unwrap();
        unsafe { slice::from_raw_parts_mut(pointer as *mut _, memory_map_size) }
    };

    let memory_map_len = memory_map_size / core::mem::size_of::<MemoryDescriptor>();
    let BootInformationAllocation {
        size: boot_info_size,
        boot_info: boot_info_uninit,
        memory_regions: memory_regions_uninit,
        modules: modules_uninit,
        elf_sections: elf_sections_uninit,
        kernel_mappings,
    } = allocate_boot_info(memory, memory_map_len, modules, elf_sections);

    let memory_map = system_table
        .exit_boot_services(handle, memory_map_storage)
        .unwrap()
        .1;

    fn convert(memory_descriptor: &MemoryDescriptor) -> MemoryRegion {
        MemoryRegion {
            start: memory_descriptor.phys_start as usize,
            len: memory_descriptor.page_count as usize * 4096,
            kind: match memory_descriptor.ty {
                MemoryType::CONVENTIONAL
                | MemoryType::LOADER_CODE
                | MemoryType::LOADER_DATA
                | MemoryType::BOOT_SERVICES_CODE
                | MemoryType::BOOT_SERVICES_DATA => MemoryRegionKind::Usable,
                tag => MemoryRegionKind::UnknownUefi(tag.0),
            },
        }
    }

    pub struct MemoryDescriptorConsolidator<'a, I>
    where
        I: ExactSizeIterator<Item = &'a MemoryDescriptor> + Clone,
    {
        inner: Peekable<I>,
    }

    impl<'a, I> Iterator for MemoryDescriptorConsolidator<'a, I>
    where
        I: ExactSizeIterator<Item = &'a MemoryDescriptor> + Clone,
    {
        type Item = MemoryRegion;

        fn next(&mut self) -> Option<Self::Item> {
            let mut area = convert(self.inner.next()?);

            // UEFI often separates contiguous memory into separate memory regions. We
            // consolidate them to minimise the number of entries in the frame allocator's
            // reserved and available lists.
            while let Some(next) = self.inner.next_if(|next| {
                let next = convert(next);
                area.kind == next.kind && (area.start + area.len) == next.start
            }) {
                let next = convert(next);
                area.len += next.len;
            }

            Some(area)
        }
    }

    let memory_map_iter = MemoryDescriptorConsolidator {
        inner: memory_map.peekable(),
    };
    let mut i = 0;
    for memory_region in memory_map_iter {
        memory_regions_uninit[i].write(memory_region);
        i += 1;
    }

    let memory_regions =
        unsafe { MaybeUninit::slice_assume_init_mut(&mut memory_regions_uninit[..i]) };
    let modules = MaybeUninit::write_slice(modules_uninit, modules);
    let elf_sections = MaybeUninit::write_slice(elf_sections_uninit, elf_sections);

    let boot_info = boot_info_uninit.write(BootInformation {
        size: boot_info_size,
        frame_buffer: mappings.frame_buffer.map(|start| FrameBuffer {
            start: start.value(),
            info: frame_buffer.unwrap().info,
        }),
        rsdp_address,
        memory_regions: unsafe {
            MemoryRegions::from_offset(
                kernel_mappings.boot_info,
                kernel_mappings.memory_regions_offset,
                memory_regions.len(),
            )
        },
        modules: unsafe {
            Modules::from_offset(
                kernel_mappings.boot_info,
                kernel_mappings.modules_offset,
                modules.len(),
            )
        },
        elf_sections: unsafe {
            ElfSections::from_offset(
                kernel_mappings.boot_info,
                kernel_mappings.elf_sections_offset,
                elf_sections.len(),
            )
        },
    });
    info!("created boot info: {boot_info:x?}");

    let context = Context {
        page_table,
        stack_top: mappings.stack_top,
        entry_point,
        boot_info: kernel_mappings.boot_info,
    };

    info!("about to switch to kernel: {context:x?}");
    unsafe { context_switch(context) };
}

fn get_frame_buffer(system_table: &SystemTable<Boot>) -> Option<FrameBuffer> {
    let handle = system_table
        .boot_services()
        .get_handle_for_protocol::<GraphicsOutput>()
        .ok()?;
    let mut gop = system_table
        .boot_services()
        .open_protocol_exclusive::<GraphicsOutput>(handle)
        .ok()?;

    let mode_info = gop.current_mode_info();
    let mut frame_buffer = gop.frame_buffer();
    let info = FrameBufferInfo {
        size: frame_buffer.size(),
        width: mode_info.resolution().0,
        height: mode_info.resolution().1,
        pixel_format: match mode_info.pixel_format() {
            gop::PixelFormat::Rgb => PixelFormat::Rgb,
            gop::PixelFormat::Bgr => PixelFormat::Bgr,
            gop::PixelFormat::Bitmask | gop::PixelFormat::BltOnly => {
                panic!("Bitmask and BltOnly framebuffers are not supported")
            }
        },
        bytes_per_pixel: 4,
        stride: mode_info.stride(),
    };

    Some(FrameBuffer {
        start: frame_buffer.as_mut_ptr() as usize,
        info,
    })
}

fn init_logger(frame_buffer: &FrameBuffer) {
    let slice = unsafe {
        core::slice::from_raw_parts_mut(frame_buffer.start as *mut _, frame_buffer.info.size)
    };
    let logger =
        logger::LOGGER.call_once(move || logger::LockedLogger::new(slice, frame_buffer.info));
    log::set_logger(logger).expect("logger already set");
    log::set_max_level(log::LevelFilter::Trace);
}

fn get_rsdp_address(system_table: &SystemTable<Boot>) -> Option<usize> {
    let mut config_entries = system_table.config_table().iter();
    // look for an ACPI2 RSDP first
    let acpi2_rsdp = config_entries.find(|entry| matches!(entry.guid, ACPI2_GUID));
    // if no ACPI2 RSDP is found, look for a ACPI1 RSDP
    let rsdp = acpi2_rsdp.or_else(|| config_entries.find(|entry| matches!(entry.guid, ACPI_GUID)));
    rsdp.map(|entry| entry.address as usize)
}

fn set_up_mappings(memory: &mut Memory, frame_buffer: &Option<FrameBuffer>) -> Mappings {
    // TODO: enable nxe and write protect bits on x86_64

    // TODO
    const STACK_SIZE: usize = 18 * 4096;

    let stack_start_address = memory.get_free_address(STACK_SIZE);

    let stack_start = Page::containing_address(stack_start_address);
    let stack_end = {
        let end_address = stack_start_address + STACK_SIZE;
        Page::containing_address(end_address - 1)
    };

    // The +1 means the guard page isn't mapped to a frame.
    for page in (stack_start + 1)..=stack_end {
        let frame = memory.allocate_frame().unwrap();
        // TODO: No execute?
        memory.map(page, frame, PteFlags::PRESENT | PteFlags::WRITABLE);
    }

    // TODO: Explain
    memory.map(
        Page::containing_address(VirtualAddress::new_canonical(context_switch as usize)),
        Frame::containing_address(PhysicalAddress::new_canonical(context_switch as usize)),
        PteFlags::PRESENT,
    );

    let frame_buffer = frame_buffer.map(|frame_buffer| {
        let start_virtual = memory.get_free_address(frame_buffer.info.size);

        let start_page = Page::containing_address(start_virtual);
        let end_page = Page::containing_address(start_virtual + frame_buffer.info.size - 1);

        let start_frame =
            Frame::containing_address(PhysicalAddress::new_canonical(frame_buffer.start));
        let end_frame = Frame::containing_address(PhysicalAddress::new_canonical(
            frame_buffer.start + frame_buffer.info.size - 1,
        ));

        for (page, frame) in (start_page..=end_page).zip(start_frame..=end_frame) {
            // We don't need to allocate frames because the frame buffer is already reserved
            // in the memory map.
            memory.map(page, frame, PteFlags::PRESENT | PteFlags::WRITABLE);
        }

        start_virtual
    });

    set_up_arch_specific_mappings(memory);

    // TODO: GDT
    // TODO: recursive index

    Mappings {
        stack_top: (stack_end + 1).start_address(),
        frame_buffer,
    }
}

struct Mappings {
    stack_top: VirtualAddress,
    frame_buffer: Option<VirtualAddress>,
}

fn allocate_boot_info(
    mut memory: Memory,
    memory_regions_count: usize,
    modules: &'static [Module],
    elf_sections: &'static [ElfSection],
) -> BootInformationAllocation {
    let boot_info_layout = Layout::new::<BootInformation>();

    let memory_regions_layout = Layout::array::<MemoryRegion>(memory_regions_count).unwrap();
    let (combined, memory_regions_offset) = boot_info_layout.extend(memory_regions_layout).unwrap();

    let modules_layout = Layout::array::<Module>(modules.len()).unwrap();
    let (combined, modules_offset) = combined.extend(modules_layout).unwrap();

    let elf_sections_layout = Layout::array::<ElfSection>(elf_sections.len()).unwrap();
    let (combined, elf_sections_offset) = combined.extend(elf_sections_layout).unwrap();

    let (start_page, end_page) = {
        let boot_info_address = memory.get_free_address(combined.size());
        let elf_sections_end = boot_info_address + combined.size();

        (
            Page::containing_address(boot_info_address),
            Page::containing_address(elf_sections_end - 1),
        )
    };

    // We want to minimise the number of frame allocations to keep
    // memory_regions_count the same.

    let frames = memory
        .allocate_frames((start_page..=end_page).count())
        .unwrap();
    // Abuse UEFI's identy-mapping
    let boot_info_address = frames.start_address();

    for (page, frame) in (start_page..=end_page).zip(frames) {
        memory.map(page, frame, PteFlags::PRESENT | PteFlags::WRITABLE);
    }

    let memory_map_regions_address = boot_info_address + memory_regions_offset;
    let modules_address = boot_info_address + modules_offset;
    let elf_sections_address = boot_info_address + elf_sections_offset;

    let boot_info: &'static mut MaybeUninit<BootInformation> =
        unsafe { &mut *(boot_info_address.value() as *mut _) };
    let memory_regions: &'static mut [MaybeUninit<MemoryRegion>] = unsafe {
        slice::from_raw_parts_mut(
            memory_map_regions_address.value() as *mut _,
            memory_regions_count,
        )
    };
    let modules: &'static mut [MaybeUninit<Module>] =
        unsafe { slice::from_raw_parts_mut(modules_address.value() as *mut _, modules.len()) };
    let elf_sections: &'static mut [MaybeUninit<ElfSection>] = unsafe {
        slice::from_raw_parts_mut(elf_sections_address.value() as *mut _, elf_sections.len())
    };

    BootInformationAllocation {
        size: combined.size(),
        boot_info,
        memory_regions,
        modules,
        elf_sections,
        kernel_mappings: BootInformationKernelMappings {
            boot_info: start_page.start_address().value() as *mut _,
            memory_regions_offset,
            modules_offset,
            elf_sections_offset,
        },
    }
}

struct BootInformationAllocation {
    size: usize,
    boot_info: &'static mut MaybeUninit<BootInformation>,
    memory_regions: &'static mut [MaybeUninit<MemoryRegion>],
    modules: &'static mut [MaybeUninit<Module>],
    elf_sections: &'static mut [MaybeUninit<ElfSection>],
    kernel_mappings: BootInformationKernelMappings,
}

#[derive(Debug)]
pub struct BootInformationKernelMappings {
    boot_info: *mut BootInformation,
    memory_regions_offset: usize,
    modules_offset: usize,
    elf_sections_offset: usize,
}

#[derive(Debug)]
struct Context {
    page_table: Frame,
    stack_top: VirtualAddress,
    entry_point: VirtualAddress,
    boot_info: *mut BootInformation,
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if let Some(mut system_table_pointer) = unsafe { SYSTEM_TABLE } {
        let system_table = unsafe { system_table_pointer.as_mut() };
        let _ = writeln!(system_table.stdout(), "{info}");
    }

    if let Some(logger) = logger::LOGGER.get() {
        unsafe { logger.force_unlock() };
    }
    error!("{info}");

    arch::halt();
}
