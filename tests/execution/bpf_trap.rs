// `abort_immediate` becomes `llvm.trap`, which LLVM lowers to a call
// to `__bpf_trap`. The linked program must fail through the `abort` syscall
// on that path and run normally on the other.
//
// run: 0000000000000000 => error abort syscall
// run: 0100000000000000 => 1
// run: efcdab8967452301 => 123456789abcdef

#![no_std]
#![no_main]
#![feature(abort_immediate)]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub fn entrypoint(input: *const u64) -> u64 {
    let value = unsafe { core::ptr::read_volatile(input) };
    if value == 0 {
        core::process::abort_immediate();
    }
    value
}
