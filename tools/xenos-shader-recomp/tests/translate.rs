//! Translator tests on hand-built microcode (no game data).

use xenos_shader_recomp::{Stage, translate, ucode, validate};

/// A CF slot holding `exec_end` of `count` instructions at `address`
/// (`sequence` 2 bits each, bit 0 = fetch), then a nop.
fn exec_end(address: u32, count: u32, sequence: u32) -> [u32; 3] {
    let a = address as u64 | (count as u64) << 12 | (sequence as u64) << 16 | 2u64 << 44;
    [a as u32, (a >> 32) as u32, 0]
}

fn bytes(slots: &[[u32; 3]]) -> Vec<u8> {
    slots.iter().flat_map(|s| s.iter().flat_map(|w| w.to_be_bytes())).collect()
}

/// vfetch r<dst> from fetch constant 95 indexed by r0.x, format 57 (32_32_32_FLOAT).
fn vfetch(dst: u32, format: u32, stride: u32) -> [u32; 3] {
    let w0 = (dst << 12) | 1 << 19 | 31 << 20 | 2 << 25;
    let w1 = 0x688 | format << 16;
    [w0, w1, stride]
}

/// max oPos, r1, r1 (export of r1 to the position).
const EXPORT_POS: [u32; 3] = [0xC80F_803E, 0, 0xE201_0100];

#[test]
fn identity_ignores_patched_fetch_fields() {
    let shipped = bytes(&[exec_end(1, 2, 0b01), vfetch(1, 0, 0), EXPORT_POS]);
    // D3D's patch: format, flags, prefetch count, mini bit, stride, offset.
    let mut patched_fetch = vfetch(1, 57, 3);
    patched_fetch[0] |= 0x3800_0000;
    patched_fetch[1] |= 0x4000_3000;
    patched_fetch[2] |= 6 << 8;
    let patched = bytes(&[exec_end(1, 2, 0b01), patched_fetch, EXPORT_POS]);
    assert_eq!(ucode::identity(&shipped), ucode::identity(&patched));
    // A different destination register is a different shader.
    let other = bytes(&[exec_end(1, 2, 0b01), vfetch(2, 0, 0), EXPORT_POS]);
    assert_ne!(ucode::identity(&shipped), ucode::identity(&other));
}

#[test]
fn identity_ignores_fetch_order_and_descriptors_follow_keys() {
    // Two fetches into r1 and r2; D3D's patch may swap them between slots and
    // make the second a mini fetch of the first's stream.
    let shipped = bytes(&[exec_end(1, 3, 0b0101), vfetch(1, 0, 0), vfetch(2, 0, 0), EXPORT_POS]);
    let mut first = vfetch(2, 57, 5);
    first[0] |= 0x3800_0000;
    let mut mini = vfetch(1, 37, 0);
    mini[1] |= 1 << 30;
    mini[2] |= 3 << 8;
    let patched = bytes(&[exec_end(1, 3, 0b0101), first, mini, EXPORT_POS]);
    assert_eq!(ucode::identity(&shipped), ucode::identity(&patched));
    let d = ucode::vertex_fetch_descriptors(&ucode::slots(&patched));
    assert_eq!(d.len(), 2);
    // Keyed by the destination register's fetch, the mini one made full.
    assert_eq!(d[0].0, ucode::vertex_fetch_key(vfetch(2, 0, 0)));
    assert_eq!(d[1].0, ucode::vertex_fetch_key(vfetch(1, 0, 0)));
    assert_eq!((d[1].1[1] >> 30) & 1, 0);
    assert_eq!((d[1].1[0], d[1].1[2], d[1].1[3]), (95, 5, 3));
}

#[test]
fn fetch_descriptor_fields() {
    let mut f = vfetch(1, 57, 3);
    f[2] |= 0x7F_FFFF << 8; // offset -1
    let d = ucode::vertex_fetch_descriptor(f);
    assert_eq!(d[0], 95);
    assert_eq!((d[1] >> 16) & 0x3F, 57);
    assert_eq!(d[2], 3);
    assert_eq!(d[3] as i32, -1);
}

#[test]
fn vertex_shader_translates_and_validates() {
    let code = bytes(&[exec_end(1, 2, 0b01), vfetch(1, 0, 0), EXPORT_POS]);
    let t = translate(&code, Stage::Vertex).expect("translate");
    validate(&t.wgsl).unwrap_or_else(|e| panic!("{e}\n{}", t.wgsl));
    assert_eq!(t.reflection.vertex_fetch_slots, vec![1]);
    assert!(t.wgsl.contains("opos = vres"));
}

#[test]
fn container_microcode_and_literals() {
    // A pixel-shader container laid out like the game's: definition table
    // (0x28 bytes) at +0x24, shader record at +0x4C, 0x58-byte virtual part;
    // physical part = one literal (c252, 4 floats) then one microcode slot.
    let mut c = vec![0u8; 0x58];
    let put = |c: &mut Vec<u8>, o: usize, v: u32| c[o..o + 4].copy_from_slice(&v.to_be_bytes());
    put(&mut c, 0, 0x102A_1100);
    put(&mut c, 4, 0x58); // virtual size
    put(&mut c, 8, 16 + 12); // physical size
    put(&mut c, 0x14, 0x24); // definition table
    put(&mut c, 0x18, 0x4C); // shader record
    put(&mut c, 0x24 + 4, 1);
    put(&mut c, 0x24 + 0x10, 0x14); // float section size
    put(&mut c, 0x24 + 0x14, 0x01FC_0004); // register 256 + 252, 4 floats (one vec4)
    put(&mut c, 0x4C, 16); // microcode offset in the physical part
    put(&mut c, 0x50, 12); // microcode size
    for f in [1.0f32, 2.0, 3.0, 4.0] {
        c.extend(f.to_bits().to_be_bytes());
    }
    c.extend(bytes(&[EXPORT_POS]));
    let found = xenos_shader_recomp::containers(&c);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].stage, Stage::Pixel);
    assert_eq!(&c[found[0].code.clone()], &bytes(&[EXPORT_POS])[..]);
    assert_eq!(found[0].literals, vec![(252, [1.0f32.to_bits(), 2.0f32.to_bits(), 3.0f32.to_bits(), 4.0f32.to_bits()])]);
}

#[test]
fn pixel_shader_export_and_constant_multiply() {
    // mul oC0, r0, c1 (vector op 1; src1 = r0 temp, src2 = c1 constant).
    let mul = [0xC80F_8000, 0, 0x8100_0100 | 1 << 29 | 0x0100_0000];
    let code = bytes(&[exec_end(1, 1, 0), mul]);
    let t = translate(&code, Stage::Pixel).expect("translate");
    validate(&t.wgsl).unwrap_or_else(|e| panic!("{e}\n{}", t.wgsl));
    assert!(t.reflection.colors.contains(&0));
    assert!(t.wgsl.contains("xmul("));
}

#[test]
fn scalar_export_goes_to_the_export_register() {
    // From the game's EDRAM restore shader: `maxs_sat` exported with vector
    // destination 61 (the depth export), vector mask 0, scalar mask x, and the
    // scalar destination field 0. An exporting instruction has one destination,
    // so the scalar result is the depth, not colour 0.
    let depth = [0x1610_803D, 0x0000_006C, 0xE200_0000];
    let colour = [0xC80F_8000, 0x0000_0000, 0xE201_0100];
    let code = bytes(&[exec_end(1, 2, 0), depth, colour]);
    let t = translate(&code, Stage::Pixel).expect("translate");
    validate(&t.wgsl).unwrap_or_else(|e| panic!("{e}\n{}", t.wgsl));
    assert!(t.reflection.writes_depth, "depth export missing:\n{}", t.wgsl);
    assert!(t.wgsl.contains("odepth.x = "), "{}", t.wgsl);
    assert!(!t.wgsl.contains("oc[0].x = (vec4<f32>(sres))"), "{}", t.wgsl);
}

#[test]
fn cond_exec_pred_clean_tests_a_bool_constant() {
    // CF opcodes 13/14 (`CondExecPredClean`) are conditional on a bool
    // constant (address bits 34-41, condition bit 42), like 3/4: the game's
    // skinning shaders choose their four-bone path with them, and never set
    // the predicate. Opcodes 5/6 test the predicate.
    let cf = |opcode: u64| (opcode << 44) | (1 << 42) | (1 << 34) | (2 << 12) | 7;
    match ucode::decode_cf(cf(13)) {
        ucode::Cf::Exec { address: 7, count: 2, end: false, cond: ucode::ExecCond::Bool { index: 1, value: true }, .. } => {}
        other => panic!("opcode 13: {other:?}"),
    }
    match ucode::decode_cf(cf(14)) {
        ucode::Cf::Exec { end: true, cond: ucode::ExecCond::Bool { index: 1, value: true }, .. } => {}
        other => panic!("opcode 14: {other:?}"),
    }
    match ucode::decode_cf(cf(5)) {
        ucode::Cf::Exec { cond: ucode::ExecCond::Pred { value: true }, .. } => {}
        other => panic!("opcode 5: {other:?}"),
    }
}

/// A CF slot holding two 48-bit CF instructions.
fn cf_pair(a: u64, b: u64) -> [u32; 3] {
    [a as u32, ((a >> 32) & 0xFFFF) as u32 | ((b & 0xFFFF) as u32) << 16, (b >> 16) as u32]
}

#[test]
fn forward_jumps_need_no_state_machine() {
    // CF0: jump to CF2 if bool 0; CF1: exec slot 2; CF2: exec_end slot 3. Every
    // jump is forward, so the blocks run in order under `pc` guards, with no
    // `loop { switch pc }` (which drivers compile very slowly).
    let jump = 2 | (1u64 << 42) | (11u64 << 44);
    let exec = 2 | (1u64 << 12) | (1u64 << 44);
    let exec_end = 3 | (1u64 << 12) | (2u64 << 44);
    let mul = [0xC80F_8000, 0, 0x8100_0100 | 1 << 29 | 0x0100_0000];
    let code = bytes(&[cf_pair(jump, exec), cf_pair(exec_end, 0), mul, mul]);
    let t = translate(&code, Stage::Pixel).expect("translate");
    validate(&t.wgsl).unwrap_or_else(|e| panic!("{e}\n{}", t.wgsl));
    assert!(t.wgsl.contains("if (pc == 0u)") && t.wgsl.contains("if (pc == 2u)"), "{}", t.wgsl);
    assert!(!t.wgsl.contains("switch pc"), "{}", t.wgsl);
}

#[test]
fn scalar_constant_ops_pack_the_temporary_index() {
    // From the skin shader `a9c88138`: `mul r16.xyz, r1.yyyy, r11.zyxx` with
    // `mulsc1 r15.w` (scalar op 43) of c199.x and a temporary. The temporary's
    // index is bit 0 from the opcode (_1), bit 1 the src3 select bit and bits 2-5
    // from the src3 swizzle: r7. It was read as r5 relative to the loop counter,
    // which turned the wrestlers' skin orange in close-ups.
    let alu = [0xAC87_0F10, 0x00B1_6245, 0xE101_0BC7];
    let code = bytes(&[exec_end(1, 1, 0), alu]);
    let t = translate(&code, Stage::Pixel).expect("translate");
    validate(&t.wgsl).unwrap_or_else(|e| panic!("{e}\n{}", t.wgsl));
    assert!(t.wgsl.contains("let sa = kc(199u).x; let sb = r[7].y;"), "{}", t.wgsl);
}

#[test]
fn literal_runs_have_their_own_offsets() {
    // The skinning shader `5f05be01`'s table: two runs, c240-c243 at offset 0
    // and c252 at 0x40 of the physical part, an empty entry between them.
    // Entries are 8 bytes (register/count, byte offset), not 4.
    let mut c = vec![0u8; 0x60];
    let put = |c: &mut Vec<u8>, o: usize, v: u32| c[o..o + 4].copy_from_slice(&v.to_be_bytes());
    put(&mut c, 0, 0x102A_1101);
    put(&mut c, 4, 0x60); // virtual size
    put(&mut c, 8, 0x50 + 12); // physical size: 5 vec4 literals, then microcode
    put(&mut c, 0x14, 0x24); // definition table
    put(&mut c, 0x18, 0x54); // shader record
    put(&mut c, 0x24 + 0x10, 0x1C); // float section size
    put(&mut c, 0x24 + 0x14, 0x00F0_0010); // c240, 16 floats
    put(&mut c, 0x24 + 0x18, 0);
    put(&mut c, 0x24 + 0x1C, 0x00FC_0004); // c252, 4 floats
    put(&mut c, 0x24 + 0x20, 0x40);
    put(&mut c, 0x54, 0x50); // microcode offset in the physical part
    put(&mut c, 0x58, 12); // microcode size
    for i in 0..20 {
        c.extend((i as f32).to_bits().to_be_bytes());
    }
    c.extend(bytes(&[EXPORT_POS]));
    let found = xenos_shader_recomp::containers(&c);
    assert_eq!(found.len(), 1);
    let regs: Vec<u32> = found[0].literals.iter().map(|l| l.0).collect();
    assert_eq!(regs, vec![240, 241, 242, 243, 252]);
    let f = |i: u32| (i as f32).to_bits();
    assert_eq!(found[0].literals[4].1, [f(16), f(17), f(18), f(19)], "c252 comes from offset 0x40");
}
