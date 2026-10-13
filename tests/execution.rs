#![allow(unused_crate_dependencies, reason = "used in test harness")]

//! Runs linked fixtures in the Solana SBPF virtual machine.
//!
//! Assembly fixtures pin down what the linker emits; these pin down what the
//! emitted program computes. Each file in `tests/execution` is a tiny Rust
//! crate exporting `entrypoint(input: *const u8) -> u64`. It is compiled and
//! linked for every SBPF arch, verified, and executed once per `run:`
//! directive:
//!
//! ```text
//! // run: <input bytes as hex> => <returned value as hex>
//! // run: <input bytes as hex> => error <substring of the VM error>
//! ```
//!
//! `// ignore-sbpf-arch: v0` (or `v3`) skips a fixture for one arch, as in the
//! assembly fixtures. Run just this harness with:
//!
//! `cargo test --test execution -- --nocapture`

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    ptr,
    sync::Arc,
};

use solana_sbpf::{
    aligned_memory::AlignedMemory,
    declare_builtin_function, ebpf,
    elf::Executable,
    memory_region::{MemoryMapping, MemoryRegion},
    program::{BuiltinProgram, SBPFVersion},
    verifier::RequisiteVerifier,
    vm::{CallFrame, Config, ContextObject, EbpfVm, ExecutionMode},
};

const TARGET: &str = "bpfel-unknown-none";
const INSTRUCTION_BUDGET: u64 = 1_000_000;

#[derive(Clone, Copy)]
struct Arch {
    name: &'static str,
    cpu: &'static str,
    version: SBPFVersion,
    /// Instruction forms LLVM must not emit for this arch.
    llvm_args: &'static [&'static str],
}

const ARCHES: [Arch; 2] = [
    Arch { name: "v0", cpu: "v2", version: SBPFVersion::V0, llvm_args: &[] },
    Arch {
        name: "v3",
        cpu: "v4",
        version: SBPFVersion::V3,
        llvm_args: &["--disable-gotox", "--disable-ldsx", "--disable-movsx"],
    },
];

enum Expected {
    Value(u64),
    Error(String),
}

struct Run {
    line: usize,
    input: Vec<u8>,
    expected: Expected,
}

struct Fixture {
    path: PathBuf,
    name: String,
    ignored_arches: Vec<String>,
    runs: Vec<Run>,
}

struct Context {
    remaining: u64,
    memory_mapping: MemoryMapping,
}

impl ContextObject for Context {
    fn consume(&mut self, amount: u64) {
        self.remaining = self.remaining.saturating_sub(amount);
    }

    fn get_remaining(&self) -> u64 {
        self.remaining
    }

    fn active_mapping_ptr(&mut self) -> ptr::NonNull<MemoryMapping> {
        ptr::NonNull::from_mut(&mut self.memory_mapping)
    }
}

declare_builtin_function!(
    /// The `abort` syscall: unconditionally fails the program.
    SyscallAbort,
    fn rust(
        _context: &mut Context,
        _arg1: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        Err("abort syscall".into())
    }
);

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

fn directive<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.trim_start()
        .strip_prefix("//")?
        .trim_start()
        .strip_prefix(name)?
        .strip_prefix(':')
        .map(str::trim)
}

fn load_fixture(path: &Path) -> Fixture {
    let source = fs::read_to_string(path).unwrap();
    let mut ignored_arches = Vec::new();
    let mut runs = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let line_number = index + 1;
        if let Some(arches) = directive(line, "ignore-sbpf-arch") {
            ignored_arches.extend(arches.split([',', ' ']).map(str::to_owned));
        }
        let Some(run) = directive(line, "run") else { continue };
        let invalid = || -> ! {
            panic!("{}:{line_number}: invalid run directive", path.display())
        };
        let Some((input, expected)) = run.split_once("=>") else { invalid() };
        let Some(input) = parse_hex(input) else { invalid() };
        let expected = expected.trim();
        let expected = match expected.strip_prefix("error") {
            Some(error) => Expected::Error(error.trim().to_owned()),
            None => match u64::from_str_radix(expected, 16) {
                Ok(value) => Expected::Value(value),
                Err(_) => invalid(),
            },
        };
        runs.push(Run { line: line_number, input, expected });
    }
    assert!(!runs.is_empty(), "{}: no run directives", path.display());
    Fixture {
        path: path.to_owned(),
        name: path.file_stem().unwrap().to_string_lossy().into_owned(),
        ignored_arches,
        runs,
    }
}

fn rustc() -> Command {
    Command::new(
        env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")),
    )
}

fn sysroot(root_dir: &Path) -> PathBuf {
    if let Some(sysroot) = env::var_os("BPFEL_SYSROOT_DIR") {
        return PathBuf::from(sysroot);
    }
    let rustc_src = rustc_build_sysroot::rustc_sysroot_src(rustc())
        .expect("could not determine sysroot source directory");
    let directory = root_dir.join("target/sysroot");
    let mut cargo = Command::new(
        env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")),
    );
    cargo.env("RUSTC_BOOTSTRAP", "1");
    rustc_build_sysroot::SysrootBuilder::new(&directory, TARGET)
        .cargo(cargo)
        .build_mode(rustc_build_sysroot::BuildMode::Build)
        .sysroot_config(rustc_build_sysroot::SysrootConfig::NoStd)
        .build_from_source(&rustc_src)
        .expect("failed to build sysroot");
    directory
}

/// Compiles and links `fixture`, returning the SBPF program.
fn build(fixture: &Fixture, arch: Arch, sysroot: &Path) -> Vec<u8> {
    let out_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("execution")
        .join(arch.name)
        .join(&fixture.name);
    fs::create_dir_all(&out_dir).unwrap();
    let program = out_dir.join("program.so");
    let _ = fs::remove_file(&program);

    let mut command = rustc();
    command
        .arg(&fixture.path)
        .args(["--crate-type", "bin", "--target", TARGET])
        .arg("--sysroot")
        .arg(sysroot)
        .arg("-o")
        .arg(out_dir.join("program.o"))
        .args(["-C", "opt-level=3", "-C", "panic=abort"])
        .args(["-C", "relocation-model=static"])
        .arg(format!("-Clinker={}", env!("CARGO_BIN_EXE_sbpf-linker")))
        .arg(format!("-Ctarget-cpu={}", arch.cpu))
        .arg("-Ctarget-feature=+allows-misaligned-mem-access")
        .arg(format!("-Clink-arg=--arch={}", arch.name))
        .arg("-Clink-arg=--deploy=false")
        .arg("-Clink-arg=--llvm-args=--bpf-stack-size=4096");
    for llvm_arg in arch.llvm_args {
        command.arg(format!("-Clink-arg=--llvm-args={llvm_arg}"));
    }
    let output = command.output().expect("failed to run rustc");
    assert!(
        output.status.success(),
        "{} failed to build for {}:\n{}",
        fixture.path.display(),
        arch.name,
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(&program).unwrap()
}

fn execute(
    executable: &Executable<Context>,
    input: &[u8],
) -> Result<u64, String> {
    let config = executable.get_config();
    let version = executable.get_sbpf_version();
    let mut input = input.to_vec();
    let mut stack = AlignedMemory::<{ ebpf::HOST_ALIGN }>::zero_filled(
        config.stack_size(),
    );
    let mut heap = AlignedMemory::<{ ebpf::HOST_ALIGN }>::with_capacity(0);
    let stack_len = stack.len();
    let regions = vec![
        executable.get_ro_region(),
        MemoryRegion::new_gapped(
            &raw mut *stack.as_slice_mut(),
            ebpf::MM_STACK_START,
            if version.stack_frame_gaps() && config.enable_stack_frame_gaps {
                config.stack_frame_size as u64
            } else {
                0
            },
        ),
        MemoryRegion::new(&raw mut *heap.as_slice_mut(), ebpf::MM_HEAP_START),
        MemoryRegion::new(&raw mut input[..], ebpf::MM_INPUT_START),
    ];
    // SAFETY: every region outlives the mapping, which is dropped with
    // `context` at the end of this function.
    let memory_mapping =
        unsafe { MemoryMapping::new(regions, config, version) }.unwrap();
    let mut context =
        Context { remaining: INSTRUCTION_BUDGET, memory_mapping };
    let mut call_frames = vec![CallFrame::default(); config.max_call_depth];
    let mut vm = EbpfVm::new(
        executable.get_loader().clone(),
        version,
        &mut context,
        stack_len,
    );
    vm.registers[1] = ebpf::MM_INPUT_START;
    let (_, result) = vm.execute_program(
        executable,
        &mut ExecutionMode::Interpreted,
        &mut call_frames,
    );
    let result: Result<u64, _> = result.into();
    result.map_err(|error| format!("{error:?}"))
}

fn run_fixture(fixture: &Fixture, arch: Arch, sysroot: &Path) -> Vec<String> {
    let elf = build(fixture, arch, sysroot);
    let mut loader = BuiltinProgram::new_loader(Config {
        enabled_sbpf_versions: SBPFVersion::V0..=SBPFVersion::V4,
        ..Config::default()
    });
    loader.register_definition::<SyscallAbort>("abort").unwrap();
    let context = |problem: &str| {
        format!("{} [{}]: {problem}", fixture.path.display(), arch.name)
    };
    let executable =
        match Executable::<Context>::from_elf(&elf, Arc::new(loader)) {
            Ok(executable) => executable,
            Err(error) => return vec![context(&format!("load: {error:?}"))],
        };
    if executable.get_sbpf_version() != arch.version {
        return vec![context(&format!(
            "linked as {:?}",
            executable.get_sbpf_version()
        ))];
    }
    if let Err(error) = executable.verify::<RequisiteVerifier>() {
        return vec![context(&format!("verify: {error:?}"))];
    }

    let mut failures = Vec::new();
    for run in &fixture.runs {
        let actual = execute(&executable, &run.input);
        let matches = match (&run.expected, &actual) {
            (Expected::Value(expected), Ok(actual)) => expected == actual,
            (Expected::Error(expected), Err(actual)) => {
                actual.contains(expected)
            }
            _ => false,
        };
        if !matches {
            let actual = match actual {
                Ok(value) => format!("{value:#x}"),
                Err(error) => format!("error {error}"),
            };
            failures.push(format!(
                "{}:{} [{}]: got {actual}",
                fixture.path.display(),
                run.line,
                arch.name
            ));
        }
    }
    failures
}

#[test]
fn execution_test() {
    let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut fixtures = fs::read_dir(root_dir.join("tests/execution"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect::<Vec<_>>();
    fixtures.sort();
    if fixtures.is_empty() {
        return;
    }
    let sysroot = sysroot(root_dir);

    let mut failures = Vec::new();
    for path in fixtures {
        let fixture = load_fixture(&path);
        for arch in ARCHES {
            if fixture.ignored_arches.iter().any(|name| name == arch.name) {
                continue;
            }
            let fixture_failures = run_fixture(&fixture, arch, &sysroot);
            println!(
                "test [execution] {} [{}] ... {}",
                fixture.name,
                arch.name,
                if fixture_failures.is_empty() { "ok" } else { "FAILED" }
            );
            failures.extend(fixture_failures);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
