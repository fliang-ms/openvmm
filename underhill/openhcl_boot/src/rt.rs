// Copyright (C) Microsoft Corporation. All rights reserved.

//! Architecture-independent runtime support.

// This must match the hardcoded value set at the entry point in the asm.
const STACK_SIZE: usize = 32768;

#[repr(C, align(16))]
pub struct Stack([u8; STACK_SIZE]);

pub static mut STACK: Stack = Stack([0; STACK_SIZE]);

#[cfg_attr(test, allow(dead_code))]
/// Validate the stack cookie is still present. Panics if overwritten.
pub fn verify_stack_cookie() {
    // SAFETY: It's possible we've overrun the stack at this point if any
    // previous stack frame was too large. But, we know the pointer is valid and
    // never came from a rust reference, and we're about to crash if the value
    // is bogus.
    unsafe {
        let stack_ptr = core::ptr::addr_of!(STACK).cast::<u32>();
        if core::ptr::read(stack_ptr) != 0x30405060 {
            panic!("Stack was overrun - check for large variables");
        }
    }
}

/// The entry point.
///
/// X64: The relative offset for shim parameters are passed in the rsi register.
/// rax contains the base address of where the shim was loaded at.
///
/// ARM64: The relative offset for shim parameters are passed in the x1 register.
/// x2 contains the base address of where the shim was loaded at.
#[allow(dead_code)]
pub unsafe extern "C" fn start(_: usize, shim_params_offset: isize) -> ! {
    crate::shim_main(shim_params_offset)
}

#[cfg(minimal_rt)]
mod instead_of_builtins {
    #[panic_handler]
    fn panic(panic: &core::panic::PanicInfo<'_>) -> ! {
        crate::boot_logger::log!("{panic}");
        // The stack is identity mapped.
        minimal_rt::enlightened_panic::report(panic, |va| Some(va as usize));
        minimal_rt::arch::fault();
    }
}
