pub const PATCH_LEN: usize = 26;
const CALL_LEN: usize = 5;
const VIEW_SETUP_MOVE_OFFSET: usize = CALL_LEN;
const VIEW_SETUP_MOVE_LEN: usize = 3;
const JMP_OFFSET: usize = 21;
const JMP_LEN: usize = 5;

// This is the stable shape of the autodirector observer override block. The
// register that holds viewSetup is intentionally wildcarded and then decoded.
const PATTERN: &[Option<u8>] = &[
    Some(0xe8), // call get observer state
    None,
    None,
    None,
    None,
    None, // REX.W: mov rdx,<register>
    Some(0x8b),
    None, // ModRM is validated below
    Some(0x48),
    Some(0x8b),
    Some(0x08), // mov rcx,[rax]
    Some(0x4c),
    Some(0x8b),
    Some(0x41),
    Some(0x28), // mov r8,[rcx+28h]
    Some(0x48),
    Some(0x8b),
    Some(0xc8), // mov rcx,rax
    Some(0x41),
    Some(0xff),
    Some(0xd0), // call r8
    Some(0xe9), // jmp past normal view setup
    None,
    None,
    None,
    None,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnalysisStatus {
    Compatible,
    NotFound,
    Ambiguous,
    UnsupportedViewSetupMove,
    CallTargetOutsideText,
    JumpTargetOutsideText,
}

impl AnalysisStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compatible => "compatible",
            Self::NotFound => "not_found",
            Self::Ambiguous => "ambiguous",
            Self::UnsupportedViewSetupMove => "unsupported_view_setup_move",
            Self::CallTargetOutsideText => "call_target_outside_text",
            Self::JumpTargetOutsideText => "jump_target_outside_text",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnalysisReport {
    pub status: AnalysisStatus,
    pub matches: usize,
    pub patch_offset: Option<usize>,
    pub view_setup_argument_move: Option<[u8; VIEW_SETUP_MOVE_LEN]>,
    pub get_observer_state_offset: Option<usize>,
    pub original_jump_target_offset: Option<usize>,
}

impl AnalysisReport {
    pub const fn is_compatible(self) -> bool {
        matches!(self.status, AnalysisStatus::Compatible)
    }
}

pub fn analyze_autodirector_view_override(text: &[u8]) -> AnalysisReport {
    let (matches, unique_offset) = find_unique_pattern(text, PATTERN);
    let Some(patch_offset) = unique_offset else {
        return AnalysisReport {
            status: if matches == 0 {
                AnalysisStatus::NotFound
            } else {
                AnalysisStatus::Ambiguous
            },
            matches,
            patch_offset: None,
            view_setup_argument_move: None,
            get_observer_state_offset: None,
            original_jump_target_offset: None,
        };
    };

    let block = &text[patch_offset..patch_offset + PATCH_LEN];
    let view_setup_argument_move = [
        block[VIEW_SETUP_MOVE_OFFSET],
        block[VIEW_SETUP_MOVE_OFFSET + 1],
        block[VIEW_SETUP_MOVE_OFFSET + 2],
    ];
    if !is_view_setup_argument_move(view_setup_argument_move) {
        return report_with_move(
            AnalysisStatus::UnsupportedViewSetupMove,
            matches,
            patch_offset,
            view_setup_argument_move,
            None,
            None,
        );
    }

    let Some(get_observer_state_offset) = relative_target(text, patch_offset, CALL_LEN) else {
        return report_with_move(
            AnalysisStatus::CallTargetOutsideText,
            matches,
            patch_offset,
            view_setup_argument_move,
            None,
            None,
        );
    };
    let Some(original_jump_target_offset) =
        relative_target(text, patch_offset + JMP_OFFSET, JMP_LEN)
    else {
        return report_with_move(
            AnalysisStatus::JumpTargetOutsideText,
            matches,
            patch_offset,
            view_setup_argument_move,
            Some(get_observer_state_offset),
            None,
        );
    };

    report_with_move(
        AnalysisStatus::Compatible,
        matches,
        patch_offset,
        view_setup_argument_move,
        Some(get_observer_state_offset),
        Some(original_jump_target_offset),
    )
}

fn find_unique_pattern(haystack: &[u8], pattern: &[Option<u8>]) -> (usize, Option<usize>) {
    if pattern.is_empty() || pattern.len() > haystack.len() {
        return (0, None);
    }
    let mut matches = 0;
    let mut first = None;
    for (offset, window) in haystack.windows(pattern.len()).enumerate() {
        if pattern
            .iter()
            .zip(window)
            .all(|(expected, actual)| expected.is_none_or(|byte| byte == *actual))
        {
            matches += 1;
            first = Some(offset);
        }
    }
    (matches, (matches == 1).then_some(first).flatten())
}

fn report_with_move(
    status: AnalysisStatus,
    matches: usize,
    patch_offset: usize,
    view_setup_argument_move: [u8; VIEW_SETUP_MOVE_LEN],
    get_observer_state_offset: Option<usize>,
    original_jump_target_offset: Option<usize>,
) -> AnalysisReport {
    AnalysisReport {
        status,
        matches,
        patch_offset: Some(patch_offset),
        view_setup_argument_move: Some(view_setup_argument_move),
        get_observer_state_offset,
        original_jump_target_offset,
    }
}

fn is_view_setup_argument_move(bytes: [u8; VIEW_SETUP_MOVE_LEN]) -> bool {
    let rex_w_with_optional_source_extension = matches!(bytes[0], 0x48 | 0x49);
    let moves_a_register_into_rdx = bytes[1] == 0x8b && (bytes[2] & 0xf8) == 0xd0;
    rex_w_with_optional_source_extension && moves_a_register_into_rdx
}

fn relative_target(
    text: &[u8],
    instruction_offset: usize,
    instruction_len: usize,
) -> Option<usize> {
    let displacement_offset = instruction_offset.checked_add(1)?;
    let displacement = i32::from_le_bytes(
        text.get(displacement_offset..displacement_offset.checked_add(4)?)?
            .try_into()
            .ok()?,
    ) as isize;
    instruction_offset
        .checked_add(instruction_len)?
        .checked_add_signed(displacement)
        .filter(|target| *target < text.len())
}

#[cfg(test)]
mod tests {
    use super::{AnalysisStatus, PATCH_LEN, analyze_autodirector_view_override};

    fn valid_block(text: &mut [u8], offset: usize, view_setup_move: [u8; 3]) {
        let block = &mut text[offset..offset + PATCH_LEN];
        block.copy_from_slice(&[
            0xe8,
            0,
            0,
            0,
            0,
            view_setup_move[0],
            view_setup_move[1],
            view_setup_move[2],
            0x48,
            0x8b,
            0x08,
            0x4c,
            0x8b,
            0x41,
            0x28,
            0x48,
            0x8b,
            0xc8,
            0x41,
            0xff,
            0xd0,
            0xe9,
            0,
            0,
            0,
            0,
        ]);

        let call_target = offset + 64;
        let call_displacement = (call_target as isize - (offset + 5) as isize) as i32;
        block[1..5].copy_from_slice(&call_displacement.to_le_bytes());

        let jump_target = offset + 96;
        let jump_displacement = (jump_target as isize - (offset + 26) as isize) as i32;
        block[22..26].copy_from_slice(&jump_displacement.to_le_bytes());
    }

    #[test]
    fn accepts_rbx_view_setup_move_and_resolves_relative_targets() {
        let mut text = vec![0x90; 160];
        valid_block(&mut text, 16, [0x48, 0x8b, 0xd3]);

        let report = analyze_autodirector_view_override(&text);

        assert_eq!(report.status, AnalysisStatus::Compatible);
        assert_eq!(report.matches, 1);
        assert_eq!(report.patch_offset, Some(16));
        assert_eq!(report.get_observer_state_offset, Some(80));
        assert_eq!(report.original_jump_target_offset, Some(112));
    }

    #[test]
    fn rejects_an_ambiguous_match() {
        let mut text = vec![0x90; 256];
        valid_block(&mut text, 16, [0x48, 0x8b, 0xd6]);
        valid_block(&mut text, 128, [0x49, 0x8b, 0xd6]);

        let report = analyze_autodirector_view_override(&text);

        assert_eq!(report.status, AnalysisStatus::Ambiguous);
        assert_eq!(report.matches, 2);
    }
}
