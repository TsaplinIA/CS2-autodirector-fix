const AUTODIRECTOR_MODE_OFFSET: i32 = 0x38;

pub(crate) fn build_conditional_trampoline(
    get_observer_state: usize,
    normal_setup_address: usize,
    original_jump_target: usize,
    view_setup_argument_move: [u8; 3],
    disabled_mask: u32,
) -> Vec<u8> {
    let mut code = Vec::with_capacity(128);

    code.extend_from_slice(&[
        0x50, // push rax
        0x51, // push rcx
        0x52, // push rdx
        0x41, 0x50, // push r8
        0x41, 0x51, // push r9
        0x41, 0x52, // push r10
        0x41, 0x53, // push r11
        0x53, // push rbx
    ]);

    emit_mov_rax_imm64(&mut code, get_observer_state);
    code.extend_from_slice(&[0xff, 0xd0]); // call rax
    code.extend_from_slice(&[0x8b, 0x48, AUTODIRECTOR_MODE_OFFSET as u8]); // mov ecx, [rax+0x38]
    code.extend_from_slice(&[0xb8, 1, 0, 0, 0]); // mov eax, 1
    code.extend_from_slice(&[0xd3, 0xe0]); // shl eax, cl
    code.push(0xa9); // test eax, disabled_mask
    code.extend_from_slice(&disabled_mask.to_le_bytes());

    let jz_offset_position = code.len() + 2;
    code.extend_from_slice(&[0x0f, 0x84, 0, 0, 0, 0]); // jz original_path

    emit_restore_saved_registers(&mut code);
    emit_absolute_jump(&mut code, normal_setup_address);

    let original_path_offset = code.len();
    emit_restore_saved_registers(&mut code);
    emit_mov_rax_imm64(&mut code, get_observer_state);
    code.extend_from_slice(&[0xff, 0xd0]); // call rax
    code.extend_from_slice(&view_setup_argument_move); // mov rdx,<original viewSetup register>
    code.extend_from_slice(&[
        0x48, 0x8b, 0x08, // mov rcx, [rax]
        0x4c, 0x8b, 0x41, 0x28, // mov r8, [rcx+28h]
        0x48, 0x8b, 0xc8, // mov rcx, rax
        0x41, 0xff, 0xd0, // call r8
    ]);
    emit_absolute_jump(&mut code, original_jump_target);

    let after_jz = jz_offset_position + 4;
    let relative = original_path_offset as isize - after_jz as isize;
    code[jz_offset_position..jz_offset_position + 4]
        .copy_from_slice(&(relative as i32).to_le_bytes());

    code
}

pub(crate) fn emit_absolute_jump(code: &mut Vec<u8>, target: usize) {
    code.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    code.extend_from_slice(&(target as u64).to_le_bytes());
}

fn emit_restore_saved_registers(code: &mut Vec<u8>) {
    code.extend_from_slice(&[
        0x5b, // pop rbx
        0x41, 0x5b, // pop r11
        0x41, 0x5a, // pop r10
        0x41, 0x59, // pop r9
        0x41, 0x58, // pop r8
        0x5a, // pop rdx
        0x59, // pop rcx
        0x58, // pop rax
    ]);
}

fn emit_mov_rax_imm64(code: &mut Vec<u8>, value: usize) {
    code.extend_from_slice(&[0x48, 0xb8]);
    code.extend_from_slice(&(value as u64).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::{build_conditional_trampoline, emit_absolute_jump};

    #[test]
    fn absolute_jump_uses_rip_relative_indirect_encoding() {
        let mut code = Vec::new();

        emit_absolute_jump(&mut code, 0x1122_3344_5566_7788);

        assert_eq!(
            code,
            vec![
                0xff, 0x25, 0, 0, 0, 0, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
            ]
        );
    }

    #[test]
    fn conditional_trampoline_embeds_disabled_mask() {
        let code = build_conditional_trampoline(0x1000, 0x2000, 0x3000, [0x48, 0x8b, 0xd6], 0b0100);

        assert!(
            code.windows(4)
                .any(|window| window == 0b0100u32.to_le_bytes())
        );
    }

    #[test]
    fn conditional_trampoline_replays_the_current_view_setup_register_move() {
        let code = build_conditional_trampoline(0x1000, 0x2000, 0x3000, [0x48, 0x8b, 0xd3], 0b0100);

        assert!(code.windows(3).any(|window| window == [0x48, 0x8b, 0xd3]));
    }
}
