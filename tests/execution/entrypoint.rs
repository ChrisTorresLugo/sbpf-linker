// Smoke test for the execution harness itself: the program receives the
// input region in r1 and its return value is the VM result.
//
// run: 0000000000000000 => 2a
// run: 0100000000000000 => 2b
// run: ffffffffffffffff => 29
// run: 2a => error AccessViolation

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub fn entrypoint(input: *const u8) -> u64 {
    let value = unsafe { core::ptr::read_volatile(input.cast::<u64>()) };
    value.wrapping_add(42)
}
