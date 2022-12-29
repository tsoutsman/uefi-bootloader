#![allow(dead_code)]
#![feature(step_trait, abi_efiapi, maybe_uninit_slice)]
#![no_std]
#![no_main]

mod arch;
mod info;
mod kernel;
mod logger;
mod memory;
mod modules;
mod util;

use crate::{
    info::{FrameBuffer, FrameBufferInfo},
    memory::{Frame, Memory, Page, PhysicalAddress, PteFlags, VirtualAddress},
};
use core::{alloc::Layout, arch::asm, fmt::Write, mem::MaybeUninit, ptr::NonNull, slice};
use info::{BootInformation, ElfSection, MemoryRegion, Module};
use uefi::{
    prelude::entry,
    proto::console::gop::{GraphicsOutput, PixelFormat},
    table::{
        boot::{MemoryDescriptor, MemoryType},
        Boot, SystemTable,
    },
    Handle, Status,
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
        log::info!("using framebuffer at {:#x}", frame_buffer.start);
    }

    unsafe { SYSTEM_TABLE = None };

    let mut memory = Memory::new(system_table.boot_services());

    let modules = modules::load(handle, &system_table);
    log::info!("loaded modules");
    kernel::load(handle, &system_table, &mut memory);
    log::info!("loaded kernel");

    let mappings = set_up_mappings(&mut memory, &frame_buffer);
    log::info!("created memory mappings");

    let memory_map_size = system_table.boot_services().memory_map_size().map_size
        + 8 * core::mem::size_of::<MemoryDescriptor>();

    let boot_info = create_boot_info(&mut memory, mappings, modules, memory_map_size);
    log::info!("created boot info");

    let memory_map = system_table.exit_boot_services(handle, todo!());
    log::info!("exited boot services");

    let context = Context {
        page_table: todo!(),
        stack_top: todo!(),
        entry_point: todo!(),
        boot_info: todo!(),
    };

    log::info!("about to switch to kernel: {context:x?}");
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
        len: frame_buffer.size(),
        width: mode_info.resolution().0,
        height: mode_info.resolution().1,
        pixel_format: match mode_info.pixel_format() {
            PixelFormat::Rgb => info::PixelFormat::Rgb,
            PixelFormat::Bgr => info::PixelFormat::Bgr,
            PixelFormat::Bitmask | PixelFormat::BltOnly => {
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
        core::slice::from_raw_parts_mut(frame_buffer.start as *mut _, frame_buffer.info.len)
    };
    let logger =
        logger::LOGGER.call_once(move || logger::LockedLogger::new(slice, frame_buffer.info));
    log::set_logger(logger).expect("logger already set");
    log::set_max_level(log::LevelFilter::Trace);
}

fn set_up_mappings<'a, 'b>(
    memory: &'a mut Memory<'b>,
    frame_buffer: &Option<FrameBuffer>,
) -> Mappings {
    // TODO: Reserve kernel frames

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
        let start_virtual = memory.get_free_address(frame_buffer.info.len);

        let start_page = Page::containing_address(start_virtual);
        let end_page = Page::containing_address(start_virtual + frame_buffer.info.len - 1);

        let start_frame =
            Frame::containing_address(PhysicalAddress::new_canonical(frame_buffer.start));
        let end_frame = Frame::containing_address(PhysicalAddress::new_canonical(
            frame_buffer.start + frame_buffer.info.len - 1,
        ));

        for (page, frame) in (start_page..=end_page).zip(start_frame..=end_frame) {
            // We don't need to allocate frames because the frame buffer is already reserved
            // in the memory map.
            memory.map(page, frame, PteFlags::PRESENT | PteFlags::WRITABLE);
        }

        start_virtual
    });

    // TODO: GDT
    // TODO: recursive index

    Mappings {
        stack_end: (stack_end + 1).start_address(),
        frame_buffer,
    }
}

struct Mappings {
    stack_end: VirtualAddress,
    frame_buffer: Option<VirtualAddress>,
}

fn create_boot_info<'a, 'b>(
    memory: &'a mut Memory<'b>,
    mappings: Mappings,
    modules: &'static mut [Module],
    num_memory_regions: usize,
) -> &'static mut BootInformation {
    todo!();
}

unsafe fn context_switch(context: Context) -> ! {
    unsafe {
        asm!(
            "mov cr3, {}; mov rsp, {}; jmp {}",
            in(reg) context.page_table.start_address().value(),
            in(reg) context.stack_top.value(),
            in(reg) context.entry_point.value(),
            in("rdi") context.boot_info as *const _ as usize,
            options(noreturn),
        );
    }
}

#[derive(Debug)]
struct Context {
    page_table: Frame,
    stack_top: VirtualAddress,
    entry_point: VirtualAddress,
    boot_info: &'static mut BootInformation,
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
    log::error!("{info}");

    loop {
        unsafe { asm!("cli", "hlt") };
    }
}