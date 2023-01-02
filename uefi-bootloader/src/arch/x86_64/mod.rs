use crate::Context;
use core::arch::asm;

pub mod memory;

pub(crate) unsafe fn context_switch(context: Context) -> ! {
    unsafe {
        asm!(
            "mov cr3, {}; mov rsp, {}; jmp {}",
            in(reg) context.page_table.start_address().value(),
            in(reg) context.stack_top.value(),
            in(reg) context.entry_point.value(),
            in("rdi") context.boot_info,
            options(noreturn),
        );
    }
}

pub(crate) fn halt() -> ! {
    loop {
        unsafe { asm!("cli", "hlt") };
    }
}
