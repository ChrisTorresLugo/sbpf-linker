// assembly-output: ptx-linker
// ignore-sbpf-arch: v0
// revisions: div rem div_imm
// compile-flags: --crate-type bin -C opt-level=3 -C panic=abort

// LLVM's BPF v4 backend emits `sdiv`/`smod`, encoded as `div`/`mod` with the
// offset field set to 1. SBPF v3 has no signed division, so the linker lowers
// each one to an unsigned operation on the magnitudes plus a sign fixup.

#![no_std]
#![no_main]

// aux-build: loop-panic-handler.rs
extern crate loop_panic_handler;

#[unsafe(no_mangle)]
pub fn entrypoint(input: *const i64) -> i64 {
    let a = unsafe { core::ptr::read_volatile(input) };
    let b = unsafe { core::ptr::read_volatile(input.add(1)) };
    // Rule out the panicking cases so only the division itself is emitted.
    unsafe { core::hint::assert_unchecked(b != 0 && b != -1) };
    #[cfg(div)]
    return a / b;
    #[cfg(rem)]
    return a % b;
    #[cfg(div_imm)]
    return (a ^ b) / -7;
}

// CHECK: label entrypoint

// The divisor is normalized first and restored afterwards.
// div: jsge
// div: neg64
// div: jsge
// div: neg64
// div: div64
// div: neg64
// div: exit

// rem: jsge
// rem: neg64
// rem: jsge
// rem: neg64
// rem: mod64
// rem: neg64
// rem: exit

// A negative immediate becomes its magnitude and flips the quotient.
// div_imm: jsge
// div_imm: neg64
// div_imm: div64 r{{[0-9]+}}, 0x7
// div_imm: neg64
// div_imm: exit
