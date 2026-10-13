//! Lowering of BPF v4 signed division and remainder.
//!
//! LLVM's BPF backend encodes `sdiv`/`smod` as the unsigned `div`/`mod`
//! opcodes with the instruction offset field set to 1. SBPF v0 and v3 have no
//! signed division instruction, so each one is rewritten into the unsigned
//! operation on the operand magnitudes followed by a sign fixup.
//!
//! The expansion uses no scratch register and no stack slot: operands are
//! negated in place and the divisor register is restored afterwards, so the
//! only register whose value changes is the destination.

use std::collections::{HashMap, HashSet};

use either::Either;
use sbpf_assembler::SbpfArch;
use sbpf_assembler::ast::AST;
use sbpf_assembler::astnode::{ASTNode, Label};
use sbpf_common::{
    inst_param::{Number, Register},
    instruction::Instruction,
    opcode::Opcode,
};

use crate::SbpfLinkerError;

const BPF_CLASS_MASK: u8 = 0x07;
const BPF_CLASS_ALU: u8 = 0x04;
const BPF_CLASS_ALU64: u8 = 0x07;
const BPF_OP_MASK: u8 = 0xf0;
const BPF_OP_DIV: u8 = 0x30;
const BPF_OP_MOD: u8 = 0x90;
const BPF_SIGNED_OFFSET: i16 = 1;

const OPCODE_JA: u8 = 0x05;
const OPCODE_JSGE_IMM: u8 = 0x75;
const OPCODE_JSGE32_IMM: u8 = 0x76;
const OPCODE_AND64_IMM: u8 = 0x57;
const OPCODE_RSH64_IMM: u8 = 0x77;
const OPCODE_NEG32: u8 = 0x84;
const OPCODE_NEG64: u8 = 0x87;

/// If `bytes` starts with a signed `div`/`mod` instruction, returns the same
/// instruction with the signed marker cleared, i.e. its unsigned counterpart.
pub fn unsigned_form(bytes: &[u8]) -> Option<[u8; 8]> {
    let mut instruction: [u8; 8] = bytes.get(..8)?.try_into().ok()?;
    let class = instruction[0] & BPF_CLASS_MASK;
    let operation = instruction[0] & BPF_OP_MASK;
    let offset = i16::from_le_bytes([instruction[2], instruction[3]]);
    if !matches!(class, BPF_CLASS_ALU | BPF_CLASS_ALU64)
        || !matches!(operation, BPF_OP_DIV | BPF_OP_MOD)
        || offset != BPF_SIGNED_OFFSET
    {
        return None;
    }
    instruction[2..4].copy_from_slice(&0i16.to_le_bytes());
    Some(instruction)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Div,
    Rem,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Width {
    W32,
    W64,
}

struct Site {
    operation: Operation,
    width: Width,
}

fn classify(opcode: Opcode) -> Option<Site> {
    let (operation, width) = match opcode {
        Opcode::Div32Imm | Opcode::Div32Reg => (Operation::Div, Width::W32),
        Opcode::Div64Imm | Opcode::Div64Reg => (Operation::Div, Width::W64),
        Opcode::Mod32Imm | Opcode::Mod32Reg => (Operation::Rem, Width::W32),
        Opcode::Mod64Imm | Opcode::Mod64Reg => (Operation::Rem, Width::W64),
        _ => return None,
    };
    Some(Site { operation, width })
}

struct Emitter<'a> {
    arch: SbpfArch,
    origin: u64,
    next_label: usize,
    out: &'a mut Vec<ASTNode>,
    /// Byte offset of the next emitted instruction.
    cursor: u64,
}

impl Emitter<'_> {
    fn decode(&self, opcode: u8, register: u8) -> Instruction {
        let bytes = [opcode, register, 0, 0, 0, 0, 0, 0];
        let decoded = if self.arch.is_v3() {
            Instruction::from_bytes_sbpf_v3(&bytes)
        } else {
            Instruction::from_bytes(&bytes)
        };
        decoded.expect("fixup instructions are valid on every SBPF arch")
    }

    fn new_label(&mut self) -> String {
        let name =
            format!(".__sbpf_sdiv_{:x}_{}", self.origin, self.next_label);
        self.next_label += 1;
        name
    }

    fn push(&mut self, instruction: Instruction) {
        self.out
            .push(ASTNode::Instruction { instruction, offset: self.cursor });
        self.cursor += 8;
    }

    fn bind(&mut self, name: String) {
        self.out.push(ASTNode::Label {
            label: Label { name, span: 0..1 },
            offset: self.cursor,
        });
    }

    fn negate(&mut self, register: u8, width: Width) {
        let opcode = match width {
            Width::W32 => OPCODE_NEG32,
            Width::W64 => OPCODE_NEG64,
        };
        let instruction = self.decode(opcode, register);
        self.push(instruction);
    }

    /// Jump to `target` when `register` is non-negative at `width`.
    fn jump_if_non_negative(
        &mut self,
        register: u8,
        width: Width,
        target: &str,
    ) {
        let opcode = match width {
            Width::W32 => OPCODE_JSGE32_IMM,
            Width::W64 => OPCODE_JSGE_IMM,
        };
        let mut instruction = self.decode(opcode, register);
        instruction.off = Some(Either::Left(target.to_owned()));
        self.push(instruction);
    }

    fn jump(&mut self, target: &str) {
        let mut instruction = self.decode(OPCODE_JA, 0);
        instruction.off = Some(Either::Left(target.to_owned()));
        self.push(instruction);
    }

    /// `dst = sign(dst) * (|dst| op divisor)`, where the divisor operand of
    /// `unsigned` already holds a magnitude.
    fn apply_dividend_sign(
        &mut self,
        dst: u8,
        width: Width,
        unsigned: &Instruction,
    ) {
        let non_negative = self.new_label();
        let done = self.new_label();
        self.jump_if_non_negative(dst, width, &non_negative);
        self.negate(dst, width);
        self.push(unsigned.clone());
        self.negate(dst, width);
        self.jump(&done);
        self.bind(non_negative);
        self.push(unsigned.clone());
        self.bind(done);
    }
}

fn expand(
    emitter: &mut Emitter<'_>,
    mut unsigned: Instruction,
    site: &Site,
) -> Result<(), String> {
    let Site { operation, width } = *site;
    let dst = unsigned
        .dst
        .as_ref()
        .map(|Register { n }| *n)
        .ok_or("signed division has no destination register")?;

    if width == Width::W32 && !emitter.arch.is_v3() {
        return Err(String::from(
            "32-bit signed division needs jmp32, which SBPF v0 does not have",
        ));
    }

    if let Some(Register { n: src }) = unsigned.src {
        if src == dst {
            // x / x == 1 and x % x == 0 for every nonzero x, signed or not,
            // and both forms fault identically on zero.
            emitter.push(unsigned);
            return Ok(());
        }

        // Negating all 64 bits negates the low 32 bits too, and negating
        // twice restores the full register, so the divisor is always
        // normalized at 64 bits even for 32-bit operations.
        let divisor_non_negative = emitter.new_label();
        let done = emitter.new_label();
        emitter.jump_if_non_negative(src, width, &divisor_non_negative);
        emitter.negate(src, Width::W64);
        emitter.apply_dividend_sign(dst, width, &unsigned);
        if operation == Operation::Div {
            emitter.negate(dst, width);
        }
        emitter.negate(src, Width::W64);
        emitter.jump(&done);
        emitter.bind(divisor_non_negative);
        emitter.apply_dividend_sign(dst, width, &unsigned);
        emitter.bind(done);
        return Ok(());
    }

    let Some(Either::Right(Number::Int(divisor))) = unsigned.imm else {
        return Err(String::from("signed division has no divisor operand"));
    };
    let divisor = i32::try_from(divisor)
        .map_err(|_| "signed division immediate does not fit in 32 bits")?;
    if width == Width::W64 && divisor == i32::MIN {
        // The magnitude 2^31 does not fit a sign-extended 32-bit immediate,
        // but dividing by a power of two needs no division at all.
        let (opcode, operand) = match operation {
            Operation::Div => (OPCODE_RSH64_IMM, 31),
            Operation::Rem => (OPCODE_AND64_IMM, i64::from(i32::MAX)),
        };
        unsigned = emitter.decode(opcode, dst);
        unsigned.imm = Some(Either::Right(Number::Int(operand)));
    } else {
        // A 32-bit operation reads the immediate as u32, so the bit pattern
        // of i32::MIN already is its magnitude.
        let magnitude = i64::from(divisor.wrapping_abs());
        unsigned.imm = Some(Either::Right(Number::Int(magnitude)));
    }
    emitter.apply_dividend_sign(dst, width, &unsigned);
    if operation == Operation::Div && divisor < 0 {
        emitter.negate(dst, width);
    }
    Ok(())
}

/// Turns every relative jump and call target into a label so that code can be
/// inserted without invalidating encoded displacements.
fn label_relative_targets(nodes: &mut Vec<ASTNode>) -> Result<(), String> {
    let mut label_at_offset: HashMap<u64, String> = HashMap::new();
    let mut instruction_offsets: HashSet<u64> = HashSet::new();
    for node in nodes.iter() {
        match node {
            ASTNode::Label { label, offset } => {
                label_at_offset
                    .entry(*offset)
                    .or_insert_with(|| label.name.clone());
            }
            ASTNode::Instruction { offset, .. } => {
                instruction_offsets.insert(*offset);
            }
            _ => {}
        }
    }

    let mut new_labels: Vec<(u64, String)> = Vec::new();
    for node in nodes.iter_mut() {
        let ASTNode::Instruction { instruction, offset } = node else {
            continue;
        };
        let relative = if instruction.is_jump() {
            match instruction.off {
                Some(Either::Right(relative)) => i64::from(relative),
                _ => continue,
            }
        } else if instruction.opcode == Opcode::Call
            && instruction.src.as_ref().is_some_and(|src| src.n == 1)
        {
            match instruction.imm {
                Some(Either::Right(Number::Int(relative))) => relative,
                _ => continue,
            }
        } else {
            continue;
        };

        let target = relative
            .checked_add(1)
            .and_then(|slots| slots.checked_mul(8))
            .and_then(|delta| offset.checked_add_signed(delta))
            .filter(|target| instruction_offsets.contains(target))
            .ok_or_else(|| {
                format!(
                    "relative branch at offset {offset:#x} does not target an instruction"
                )
            })?;
        let name = label_at_offset
            .entry(target)
            .or_insert_with(|| {
                let name = format!(".__sbpf_sdiv_target_{target:x}");
                new_labels.push((target, name.clone()));
                name
            })
            .clone();
        if instruction.is_jump() {
            instruction.off = Some(Either::Left(name));
        } else {
            instruction.imm = Some(Either::Left(name));
        }
    }

    if new_labels.is_empty() {
        return Ok(());
    }
    let mut labels_by_offset: HashMap<u64, String> =
        new_labels.into_iter().collect();
    let mut labelled =
        Vec::with_capacity(nodes.len() + labels_by_offset.len());
    for node in nodes.drain(..) {
        if let ASTNode::Instruction { offset, .. } = &node
            && let Some(name) = labels_by_offset.remove(offset)
        {
            labelled.push(ASTNode::Label {
                label: Label { name, span: 0..1 },
                offset: *offset,
            });
        }
        labelled.push(node);
    }
    *nodes = labelled;
    Ok(())
}

/// Rewrites the signed division sites at `signed_offsets`, which must already
/// be decoded as their unsigned forms, and returns the number of bytes the
/// text section grew by.
///
/// `ast.nodes` must be in source order with every label preceding the
/// instruction at the same offset.
pub fn lower_signed_division(
    ast: &mut AST,
    signed_offsets: &HashSet<u64>,
    arch: SbpfArch,
) -> Result<u64, Vec<SbpfLinkerError>> {
    if signed_offsets.is_empty() {
        return Ok(0);
    }

    let lowering_error = |offset: u64, detail: String| {
        SbpfLinkerError::SignedDivisionError { offset, detail }
    };

    label_relative_targets(&mut ast.nodes)
        .map_err(|detail| vec![lowering_error(0, detail)])?;

    let mut errors = Vec::new();
    let mut growth = 0u64;
    let mut lowered = Vec::with_capacity(ast.nodes.len());
    for node in std::mem::take(&mut ast.nodes) {
        match node {
            ASTNode::Label { label, offset } => {
                lowered
                    .push(ASTNode::Label { label, offset: offset + growth });
            }
            ASTNode::Instruction { instruction, offset }
                if signed_offsets.contains(&offset) =>
            {
                let start = offset + growth;
                let Some(site) = classify(instruction.opcode) else {
                    errors.push(lowering_error(
                        offset,
                        String::from("instruction is not a division"),
                    ));
                    continue;
                };
                let mut emitter = Emitter {
                    arch,
                    origin: offset,
                    next_label: 0,
                    out: &mut lowered,
                    cursor: start,
                };
                match expand(&mut emitter, instruction, &site) {
                    Ok(()) => growth += emitter.cursor - start - 8,
                    Err(detail) => errors.push(lowering_error(offset, detail)),
                }
            }
            ASTNode::Instruction { instruction, offset } => {
                lowered.push(ASTNode::Instruction {
                    instruction,
                    offset: offset + growth,
                });
            }
            other => lowered.push(other),
        }
    }
    ast.nodes = lowered;

    if errors.is_empty() { Ok(growth) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLES: [i64; 22] = [
        0,
        1,
        -1,
        2,
        -2,
        7,
        -7,
        5_000,
        -5_000,
        i32::MAX as i64,
        i32::MIN as i64,
        i32::MAX as i64 + 1,
        i32::MIN as i64 - 1,
        0x1_0000_0003,
        -0x1_0000_0003,
        0x0123_4567_89ab_cdef,
        -0x0123_4567_89ab_cdef,
        0xffff_ffff,
        i64::MAX,
        i64::MIN,
        i64::MAX - 1,
        i64::MIN + 1,
    ];

    fn signed_instruction(opcode: u8, dst: u8, src: u8, imm: i32) -> [u8; 8] {
        let mut bytes = [opcode, (src << 4) | dst, 1, 0, 0, 0, 0, 0];
        bytes[4..].copy_from_slice(&imm.to_le_bytes());
        bytes
    }

    fn lower(signed: [u8; 8]) -> Vec<ASTNode> {
        let unsigned = unsigned_form(&signed).expect("a signed division");
        let instruction = Instruction::from_bytes_sbpf_v3(&unsigned).unwrap();
        let mut ast = AST::new();
        ast.nodes.push(ASTNode::Instruction { instruction, offset: 0 });
        let growth =
            lower_signed_division(&mut ast, &HashSet::from([0]), SbpfArch::V3)
                .unwrap_or_else(|errors| {
                    panic!("{}", SbpfLinkerError::format_errors(&errors))
                });
        let instructions = ast
            .nodes
            .iter()
            .filter(|node| matches!(node, ASTNode::Instruction { .. }))
            .count() as u64;
        assert_eq!(growth, (instructions - 1) * 8);
        ast.nodes
    }

    /// Executes lowered nodes with the semantics of the SBPF v3 interpreter.
    fn execute(nodes: &[ASTNode], registers: &mut [u64; 11]) {
        let label_index = |name: &str| {
            nodes
                .iter()
                .position(|node| {
                    matches!(node, ASTNode::Label { label, .. } if label.name == name)
                })
                .expect("branch target exists")
        };
        let mut pc = 0;
        while let Some(node) = nodes.get(pc) {
            pc += 1;
            let ASTNode::Instruction { instruction, .. } = node else {
                continue;
            };
            let dst = instruction.dst.as_ref().map_or(0, |r| usize::from(r.n));
            let operand = match (&instruction.src, &instruction.imm) {
                (Some(src), _) => registers[usize::from(src.n)],
                (None, Some(Either::Right(Number::Int(imm)))) => *imm as u64,
                _ => 0,
            };
            let target = || match &instruction.off {
                Some(Either::Left(name)) => label_index(name),
                other => panic!("unresolved branch target {other:?}"),
            };
            match instruction.opcode {
                Opcode::Ja => pc = target(),
                Opcode::JsgeImm => {
                    if registers[dst] as i64 >= operand as i64 {
                        pc = target();
                    }
                }
                Opcode::Jsge32Imm => {
                    if registers[dst] as i32 >= operand as i32 {
                        pc = target();
                    }
                }
                Opcode::Neg64 => {
                    registers[dst] =
                        (registers[dst] as i64).wrapping_neg() as u64;
                }
                Opcode::Neg32 => {
                    registers[dst] = u64::from(
                        (registers[dst] as i32).wrapping_neg() as u32,
                    );
                }
                Opcode::Rsh64Imm => registers[dst] >>= operand,
                Opcode::And64Imm => registers[dst] &= operand,
                Opcode::Div64Imm | Opcode::Div64Reg => {
                    registers[dst] /= operand
                }
                Opcode::Mod64Imm | Opcode::Mod64Reg => {
                    registers[dst] %= operand
                }
                Opcode::Div32Imm | Opcode::Div32Reg => {
                    registers[dst] =
                        u64::from(registers[dst] as u32 / operand as u32);
                }
                Opcode::Mod32Imm | Opcode::Mod32Reg => {
                    registers[dst] =
                        u64::from(registers[dst] as u32 % operand as u32);
                }
                opcode => panic!("unexpected opcode {opcode:?}"),
            }
        }
    }

    fn expected(operation: Operation, width: Width, a: i64, b: i64) -> u64 {
        match (operation, width) {
            (Operation::Div, Width::W64) => a.wrapping_div(b) as u64,
            (Operation::Rem, Width::W64) => a.wrapping_rem(b) as u64,
            (Operation::Div, Width::W32) => {
                u64::from((a as i32).wrapping_div(b as i32) as u32)
            }
            (Operation::Rem, Width::W32) => {
                u64::from((a as i32).wrapping_rem(b as i32) as u32)
            }
        }
    }

    const CASES: [(u8, Operation, Width); 4] = [
        (0x3c, Operation::Div, Width::W32),
        (0x3f, Operation::Div, Width::W64),
        (0x9c, Operation::Rem, Width::W32),
        (0x9f, Operation::Rem, Width::W64),
    ];

    #[test]
    fn detects_only_signed_division() {
        assert!(unsigned_form(&[0x3f, 0x10, 0, 0, 0, 0, 0, 0]).is_none());
        // `movsx` also uses the offset field but is not a division.
        assert!(unsigned_form(&[0xbf, 0x10, 8, 0, 0, 0, 0, 0]).is_none());
        assert_eq!(
            unsigned_form(&[0x97, 0x03, 1, 0, 7, 0, 0, 0]),
            Some([0x97, 0x03, 0, 0, 7, 0, 0, 0])
        );
    }

    #[test]
    fn register_forms_match_signed_arithmetic_and_preserve_the_divisor() {
        const DST: usize = 3;
        const SRC: usize = 7;
        for (opcode, operation, width) in CASES {
            let nodes =
                lower(signed_instruction(opcode, DST as u8, SRC as u8, 0));
            for a in SAMPLES {
                for b in SAMPLES {
                    let is_zero = match width {
                        Width::W32 => b as i32 == 0,
                        Width::W64 => b == 0,
                    };
                    if is_zero {
                        continue;
                    }
                    let mut registers = [0xdead_beef_u64; 11];
                    registers[DST] = a as u64;
                    registers[SRC] = b as u64;
                    execute(&nodes, &mut registers);
                    assert_eq!(
                        registers[DST],
                        expected(operation, width, a, b),
                        "opcode {opcode:#x}: {a} by {b}"
                    );
                    registers[DST] = 0xdead_beef;
                    assert_eq!(registers[SRC], b as u64);
                    assert!(
                        registers.iter().enumerate().all(|(n, value)| n
                            == SRC
                            || *value == 0xdead_beef),
                        "opcode {opcode:#x} clobbered a register"
                    );
                }
            }
        }
    }

    #[test]
    fn immediate_forms_match_signed_arithmetic() {
        const DST: usize = 5;
        for (opcode, operation, width) in CASES {
            // Clear BPF_X to select the immediate form.
            let opcode = opcode & !0x08;
            for divisor in
                [1, -1, 3, -3, 7, -7, i32::MAX, i32::MIN + 1, i32::MIN]
            {
                let nodes =
                    lower(signed_instruction(opcode, DST as u8, 0, divisor));
                for a in SAMPLES {
                    let mut registers = [0u64; 11];
                    registers[DST] = a as u64;
                    execute(&nodes, &mut registers);
                    assert_eq!(
                        registers[DST],
                        expected(operation, width, a, i64::from(divisor)),
                        "opcode {opcode:#x}: {a} by {divisor}"
                    );
                }
            }
        }
    }
}
