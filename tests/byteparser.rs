use either::Either;
use object::write::{Object, Relocation, SectionId, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationEncoding,
    RelocationFlags, RelocationKind, SectionKind, SymbolFlags, SymbolKind,
    SymbolScope,
};
use sbpf_assembler::{OptimizationConfig, SbpfArch, astnode::ASTNode};
use sbpf_common::{
    inst_param::Number, instruction::Instruction, opcode::Opcode,
};
use sbpf_linker::{
    ProgramOptions, SbpfLinkerError, byteparser::parse_bytecode,
};

const STACK_FRAME_SIZE: i32 = 4096;
const EXIT_INSTRUCTION: [u8; 8] = [0x95, 0, 0, 0, 0, 0, 0, 0];

#[test]
fn rodata_relocation_rejects_undefined_target() {
    let (mut object, rodata) = create_object_with_rodata(8);
    let undefined = object.add_symbol(Symbol {
        name: b"undefined".to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Data,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    add_rodata_relocation(&mut object, rodata, 0, undefined, 0);

    assert_rodata_relocation_error(
        object,
        0,
        "relocation target has no section",
    );
}

#[test]
fn rodata_relocation_rejects_target_outside_rodata_and_text() {
    let (mut object, rodata) = create_object_with_rodata(8);
    let data = object.add_section(
        Vec::new(),
        b".data.test".to_vec(),
        SectionKind::Data,
    );
    object.append_section_data(data, &[0; 8], 8);
    let target = object.add_symbol(Symbol {
        name: b"data_target".to_vec(),
        value: 0,
        size: 8,
        kind: SymbolKind::Data,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Section(data),
        flags: SymbolFlags::None,
    });
    add_rodata_relocation(&mut object, rodata, 0, target, 0);

    assert_rodata_relocation_error(
        object,
        0,
        "relocation target is not rodata or text",
    );
}

#[test]
fn rodata_relocation_rejects_location_out_of_bounds() {
    let (mut object, rodata) = create_object_with_rodata(8);
    let rodata_section = object.section_symbol(rodata);
    add_rodata_relocation(&mut object, rodata, 8, rodata_section, 0);

    assert_rodata_relocation_error(
        object,
        8,
        "relocation location out of bounds",
    );
}

#[test]
fn rodata_relocation_handles_interior_rodata_target() {
    let (mut object, rodata) = create_object_with_rodata(24);
    object.add_symbol(Symbol {
        name: b"data".to_vec(),
        value: 0,
        size: 24,
        kind: SymbolKind::Data,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Section(rodata),
        flags: SymbolFlags::None,
    });
    let rodata_section = object.section_symbol(rodata);
    for offset in [0, 8] {
        add_rodata_relocation(&mut object, rodata, offset, rodata_section, 16);
    }

    let program =
        parse_object(object).expect("valid rodata relocations should parse");
    let interior_labels = program
        .data_section
        .get_nodes()
        .iter()
        .filter(|node| {
            matches!(
                node,
                ASTNode::ROData { rodata, offset }
                    if rodata.name == ".rodata.__at__0x10" && *offset == 16
            )
        })
        .count();
    assert_eq!(interior_labels, 1);
}

#[test]
fn rodata_relocation_handles_interior_text_target() {
    let (mut object, rodata) = create_object_with_rodata(8);
    let text =
        object.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    object.append_section_data(text, &EXIT_INSTRUCTION.repeat(2), 8);
    object.add_symbol(Symbol {
        name: b"entrypoint".to_vec(),
        value: 0,
        size: EXIT_INSTRUCTION.len() as u64,
        kind: SymbolKind::Text,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    let text_section = object.section_symbol(text);
    add_rodata_relocation(&mut object, rodata, 0, text_section, 8);

    let program =
        parse_object(object).expect("valid text relocation should parse");
    assert!(program.code_section.get_nodes().iter().any(|node| {
        matches!(
            node,
            ASTNode::Label { label, offset }
                if label.name == ".text.__at__0x8" && *offset == 8
        )
    }));
}

#[test]
fn signed_division_is_lowered_and_branches_are_rebased() {
    // ja +1; sdiv64 r0, r1; exit
    let program = parse_text(
        &[
            [0x05, 0, 1, 0, 0, 0, 0, 0],
            [0x3f, 0x10, 1, 0, 0, 0, 0, 0],
            EXIT_INSTRUCTION,
        ],
        SbpfArch::V3,
    )
    .expect("signed division should be lowered");

    let nodes = program.code_section.get_nodes();
    let opcodes = opcodes(nodes);
    assert_eq!(opcodes.first(), Some(&Opcode::Ja));
    assert_eq!(opcodes.last(), Some(&Opcode::Exit));
    // One division per combination of operand signs.
    assert_eq!(opcodes.iter().filter(|o| **o == Opcode::Div64Reg).count(), 4);
    assert!(opcodes.contains(&Opcode::JsgeImm));
    assert!(opcodes.contains(&Opcode::Neg64));

    // The leading jump skipped exactly the division; it must still land on
    // `exit`, which moved when the division grew.
    let (exit_offset, _) = instructions(nodes)
        .into_iter()
        .find(|(_, instruction)| instruction.opcode == Opcode::Exit)
        .unwrap();
    let (_, jump) = instructions(nodes)[0].clone();
    let Some(Either::Right(relative)) = jump.off else {
        panic!("jump target should be resolved, got {:?}", jump.off);
    };
    assert_eq!((i64::from(relative) + 1) * 8, exit_offset as i64);
    assert_eq!(program.code_section.get_size(), exit_offset + 8);
}

#[test]
fn signed_division_by_the_dividend_register_needs_no_fixup() {
    // srem64 r3, r3; exit
    let program = parse_text(
        &[[0x9f, 0x33, 1, 0, 0, 0, 0, 0], EXIT_INSTRUCTION],
        SbpfArch::V3,
    )
    .expect("signed remainder should be lowered");
    assert_eq!(
        opcodes(program.code_section.get_nodes()),
        [Opcode::Mod64Reg, Opcode::Exit]
    );
}

#[test]
fn signed_32_bit_division_uses_32_bit_sign_tests() {
    // sdiv32 r0, -7; exit
    let mut division = [0x34, 0x00, 1, 0, 0, 0, 0, 0];
    division[4..].copy_from_slice(&(-7i32).to_le_bytes());
    let program = parse_text(&[division, EXIT_INSTRUCTION], SbpfArch::V3)
        .expect("signed division should be lowered");
    let nodes = program.code_section.get_nodes();
    assert_eq!(
        opcodes(nodes),
        [
            Opcode::Jsge32Imm,
            Opcode::Neg32,
            Opcode::Div32Imm,
            Opcode::Neg32,
            Opcode::Ja,
            Opcode::Div32Imm,
            Opcode::Neg32,
            Opcode::Exit,
        ]
    );
    for (_, instruction) in instructions(nodes) {
        if instruction.opcode == Opcode::Div32Imm {
            assert_eq!(instruction.imm, Some(Either::Right(Number::Int(7))));
        }
    }
}

#[test]
fn signed_32_bit_division_is_rejected_without_jmp32() {
    let errors = parse_text(
        &[[0x3c, 0x10, 1, 0, 0, 0, 0, 0], EXIT_INSTRUCTION],
        SbpfArch::V0,
    )
    .err()
    .expect("SBPF v0 cannot test the sign of a 32-bit value");
    assert!(matches!(
        errors.as_slice(),
        [SbpfLinkerError::SignedDivisionError { offset: 0, .. }]
    ));
}

#[test]
fn signed_64_bit_division_by_i32_min_uses_a_shift() {
    // 2^31 is not a positive 32-bit immediate, so the magnitude division
    // becomes a shift (and the remainder a mask).
    for (opcode, expected) in [
        (0x37, (Opcode::Rsh64Imm, 31)),
        (0x97, (Opcode::And64Imm, i64::from(i32::MAX))),
    ] {
        let mut division = [opcode, 0x00, 1, 0, 0, 0, 0, 0];
        division[4..].copy_from_slice(&i32::MIN.to_le_bytes());
        let program = parse_text(&[division, EXIT_INSTRUCTION], SbpfArch::V3)
            .expect("signed division should be lowered");
        let unsigned = instructions(program.code_section.get_nodes())
            .into_iter()
            .filter(|(_, instruction)| instruction.opcode == expected.0)
            .collect::<Vec<_>>();
        assert_eq!(unsigned.len(), 2);
        for (_, instruction) in unsigned {
            assert_eq!(
                instruction.imm,
                Some(Either::Right(Number::Int(expected.1)))
            );
        }
    }
}

fn add_entrypoint(object: &mut Object<'_>, code: &[[u8; 8]]) -> SectionId {
    let text =
        object.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    object.append_section_data(text, &code.concat(), 8);
    object.add_symbol(Symbol {
        name: b"entrypoint".to_vec(),
        value: 0,
        size: (code.len() * 8) as u64,
        kind: SymbolKind::Text,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    text
}

fn parse_text(
    code: &[[u8; 8]],
    arch: SbpfArch,
) -> Result<sbpf_assembler::ProgramLayout, Vec<SbpfLinkerError>> {
    let mut object =
        Object::new(BinaryFormat::Elf, Architecture::Sbf, Endianness::Little);
    add_entrypoint(&mut object, code);
    parse_object_for(object, arch)
}

fn instructions(nodes: &[ASTNode]) -> Vec<(u64, Instruction)> {
    nodes
        .iter()
        .filter_map(|node| match node {
            ASTNode::Instruction { instruction, offset } => {
                Some((*offset, instruction.clone()))
            }
            _ => None,
        })
        .collect()
}

fn opcodes(nodes: &[ASTNode]) -> Vec<Opcode> {
    instructions(nodes)
        .into_iter()
        .map(|(_, instruction)| instruction.opcode)
        .collect()
}

fn create_object_with_rodata(size: usize) -> (Object<'static>, SectionId) {
    let mut object =
        Object::new(BinaryFormat::Elf, Architecture::Sbf, Endianness::Little);
    let rodata = object.add_section(
        Vec::new(),
        b".rodata".to_vec(),
        SectionKind::ReadOnlyData,
    );
    object.append_section_data(rodata, &vec![0; size], 8);
    (object, rodata)
}

fn add_rodata_relocation(
    object: &mut Object<'_>,
    rodata: SectionId,
    offset: u64,
    symbol: object::write::SymbolId,
    addend: i64,
) {
    object
        .add_relocation(
            rodata,
            Relocation {
                offset,
                symbol,
                addend,
                flags: RelocationFlags::Generic {
                    kind: RelocationKind::Absolute,
                    encoding: RelocationEncoding::Generic,
                    size: 64,
                },
            },
        )
        .expect("failed to add rodata relocation");
}

fn parse_object(
    object: Object<'_>,
) -> Result<sbpf_assembler::ProgramLayout, Vec<SbpfLinkerError>> {
    parse_object_with(object, SbpfArch::V0, OptimizationConfig::enabled())
}

// Lowering tests assert on exact instruction sequences, so they run without
// the assembler's optimizer.
fn parse_object_for(
    object: Object<'_>,
    arch: SbpfArch,
) -> Result<sbpf_assembler::ProgramLayout, Vec<SbpfLinkerError>> {
    parse_object_with(object, arch, OptimizationConfig::disabled())
}

fn parse_object_with(
    object: Object<'_>,
    arch: SbpfArch,
    optimization: OptimizationConfig,
) -> Result<sbpf_assembler::ProgramLayout, Vec<SbpfLinkerError>> {
    let bytes = object.write().expect("failed to write object");
    parse_bytecode(
        &bytes,
        ProgramOptions::new(optimization, arch, STACK_FRAME_SIZE),
    )
}

fn assert_rodata_relocation_error(
    object: Object<'_>,
    expected_address: u64,
    expected_detail: &str,
) {
    let errors = match parse_object(object) {
        Ok(_) => panic!("expected rodata relocation to fail"),
        Err(errors) => errors,
    };
    let [error] = errors.as_slice() else {
        panic!("expected one error, got {errors:?}");
    };
    match error {
        SbpfLinkerError::RodataRelocationError {
            section,
            address,
            detail,
        } => {
            assert_eq!(section, ".rodata");
            assert_eq!(*address, expected_address);
            assert_eq!(detail, expected_detail);
        }
        _ => panic!("expected RodataRelocationError"),
    }
}
