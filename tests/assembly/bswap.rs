// assembly-output: ptx-linker
// ignore-sbpf-arch: v0
// compile-flags: --crate-type bin -C opt-level=3 -C panic=abort

// LLVM's BPF v4 backend emits the unconditional `bswap` instruction (opcode
// 0xd7), which SBPF does not have. The linker emits the equivalent `be`.

#![no_std]
#![no_main]

// aux-build: loop-panic-handler.rs
extern crate loop_panic_handler;

#[unsafe(no_mangle)]
pub fn entrypoint(input: *const u64) -> u64 {
    unsafe { core::ptr::read_volatile(input) }.swap_bytes()
}

// CHECK: label entrypoint
// CHECK: be64 r0
// CHECK: exit
