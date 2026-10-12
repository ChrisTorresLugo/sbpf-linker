<h1 align="center">
  SBPF Linker
</h1>
<p align="center">
  An upstream BPF linker to relink upstream BPF binaries into an SBPF V0/V3 compatible binary format.
</p>
<p align="center">
  <a href="https://codecov.io/gh/blueshift-gg/sbpf-linker">
    <img src="https://codecov.io/gh/blueshift-gg/sbpf-linker/graph/badge.svg" alt="codecov">
  </a>
</p>

## Installation

### cargo binstall (recommended for solana devs)

The recommended installation method is
[`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall sbpf-linker
```

### cargo install (requires toolchain that ships a shared LLVM library)

A source install that selects the LLVM shared library at runtime (make sure the linker is invoked from rustc nightly with minimum nightly-2026-08-05)

```sh
cargo install sbpf-linker
```

### Generate a Program

```sh
cargo generate --git https://github.com/blueshift-gg/solana-upstream-bpf-template
```

```sh
cargo +nightly build-bpf
```

## Compiler builtins

Pass `solana-compiler-builtins` as an ordinary input, ahead of
`libcompiler_builtins.rlib`. Its builtins are `weak_odr` and Rust's are `weak`,
so LLVM keeps whichever it saw first; arriving earlier is what makes them win,
while a program defining a builtin itself still beats both. The linker warns
when no such input is present.
