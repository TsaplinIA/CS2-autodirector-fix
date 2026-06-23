#![allow(non_snake_case)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::null_mut;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

type HModule = *mut c_void;
type Handle = *mut c_void;
type Tier0Msg = unsafe extern "C" fn(format: *const c_char, ...);

const CLIENT_DLL: &CStr = c"client.dll";
const TIER0_DLL: &CStr = c"tier0.dll";
const TIER0_MSG_EXPORT: &CStr = c"Msg";
const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;

// Inside view.cpp FUN_180b77d20. This block is only reached when
// spec_autodirector is enabled and the local observer state exists. It calls
// observerState->vfunc_0x28(viewSetup), then jumps over the normal camera setup.
const AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN: usize = 26;
const AUTODIRECTOR_VIEW_OVERRIDE_JMP_OFFSET: usize = 21;
const AUTODIRECTOR_VIEW_OVERRIDE_JMP_LEN: usize = 5;
const AUTODIRECTOR_MODE_OFFSET: i32 = 0x38;
const AUTODIRECTOR_MODE_FIXED: u32 = 1;
const AUTODIRECTOR_MODE_FIRST_PERSON: u32 = 2;
const AUTODIRECTOR_MODE_CHASE: u32 = 3;
const AUTODIRECTOR_MODE_CAMERAMAN: u32 = 4;
const DEFAULT_DISABLED_CAMERA_MASK: u32 = 1 << AUTODIRECTOR_MODE_FIRST_PERSON;
const AUTODIRECTOR_VIEW_OVERRIDE_PATTERN: &[Option<u8>] = &[
    Some(0xe8), // call FUN_180b0f460
    None,
    None,
    None,
    None,
    Some(0x48),
    Some(0x8b),
    Some(0xd6), // mov rdx,rsi
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

const DOS_SIGNATURE: u16 = 0x5a4d;
const PE_SIGNATURE: u32 = 0x0000_4550;
const DOS_LFANEW_OFFSET: usize = 0x3c;
const COFF_NUMBER_OF_SECTIONS_OFFSET: usize = 0x06;
const COFF_SIZE_OF_OPTIONAL_HEADER_OFFSET: usize = 0x14;
const PE_HEADERS_SIZE: usize = 0x18;
const SECTION_HEADER_SIZE: usize = 0x28;
const SECTION_NAME_SIZE: usize = 0x08;
const SECTION_VIRTUAL_SIZE_OFFSET: usize = 0x08;
const SECTION_VIRTUAL_ADDRESS_OFFSET: usize = 0x0c;
const SECTION_SIZE_OF_RAW_DATA_OFFSET: usize = 0x10;
const TEXT_SECTION_NAME: &[u8; SECTION_NAME_SIZE] = b".text\0\0\0";
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_READ: u32 = 0x20;
const MEM_COMMIT: u32 = 0x0000_1000;
const MEM_RESERVE: u32 = 0x0000_2000;
const MEM_RELEASE: u32 = 0x0000_8000;

static MODULE_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static PATCH_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static TRAMPOLINE_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static TRAMPOLINE_LEN: AtomicUsize = AtomicUsize::new(0);
static DISABLED_CAMERA_MASK: AtomicU32 = AtomicU32::new(DEFAULT_DISABLED_CAMERA_MASK);
static PATCHED: AtomicBool = AtomicBool::new(false);
static ORIGINAL_BYTES: [AtomicU8; AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN] =
    [const { AtomicU8::new(0) }; AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN];

pub(crate) unsafe fn process_attach(module: HModule) {
    MODULE_HANDLE.store(module, Ordering::SeqCst);
    unsafe {
        DisableThreadLibraryCalls(module);
        let mut thread_module = null_mut();
        let thread_param = if GetModuleHandleExA(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            init_thread as *const c_void as *const c_char,
            &mut thread_module,
        ) != 0
        {
            thread_module
        } else {
            null_mut()
        };
        let thread = CreateThread(null_mut(), 0, init_thread, thread_param, 0, null_mut());
        if !thread.is_null() {
            CloseHandle(thread);
        }
    }
}

pub(crate) unsafe fn process_detach() {
    unsafe {
        restore_patch();
    }
}

unsafe extern "system" fn init_thread(param: *mut c_void) -> u32 {
    let exit_code = unsafe { init_thread_body() };
    if !param.is_null() {
        unsafe {
            FreeLibraryAndExitThread(param, exit_code);
        }
    }
    exit_code
}

unsafe fn init_thread_body() -> u32 {
    log("autodirector_camera_fix: init thread started");

    for _ in 0..600 {
        let client = unsafe { GetModuleHandleA(CLIENT_DLL.as_ptr()) };
        if !client.is_null() {
            unsafe {
                initialize_patch(client);
            }
            return 0;
        }
        unsafe {
            Sleep(100);
        }
    }

    log("autodirector_camera_fix: client.dll wait timed out");
    0
}

unsafe fn initialize_patch(client: HModule) {
    let config = load_config();
    DISABLED_CAMERA_MASK.store(config.disabled_mask, Ordering::SeqCst);
    log(&format!(
        "autodirector_camera_fix: config fixed={} first_person={} chase={} cameraman={} disabled_mask={:#x}",
        config.fixed, config.first_person, config.chase, config.cameraman, config.disabled_mask
    ));

    if config.disabled_mask == 0 {
        log("autodirector_camera_fix: all camera modes are enabled; patch is not needed");
        return;
    }

    let scan_start = Instant::now();
    let Some(patch_address) = (unsafe { find_patch_address(client, scan_start) }) else {
        let elapsed = scan_start.elapsed().as_secs_f64() * 1000.0;
        log(&format!(
            "autodirector_camera_fix: signature scan failed elapsed_ms={elapsed:.3}"
        ));
        log("autodirector_camera_fix: autodirector view override signature was not found");
        return;
    };
    let scan_elapsed = scan_start.elapsed().as_secs_f64() * 1000.0;
    log(&format!(
        "autodirector_camera_fix: signature scan completed elapsed_ms={scan_elapsed:.3}"
    ));

    for (offset, original_byte) in ORIGINAL_BYTES.iter().enumerate() {
        let byte = unsafe { (patch_address as *const u8).add(offset).read() };
        original_byte.store(byte, Ordering::SeqCst);
    }

    if unsafe { write_conditional_patch(patch_address, config.disabled_mask) } {
        PATCH_ADDRESS.store(patch_address as *mut c_void, Ordering::SeqCst);
        PATCHED.store(true, Ordering::SeqCst);
        log(&format!(
            "autodirector_camera_fix: patched autodirector view override client={:#x} address={:#x} len={} disabled_mask={:#x}",
            client as usize,
            patch_address,
            AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN,
            config.disabled_mask
        ));
    } else {
        unsafe {
            free_trampoline();
        }
        log(&format!(
            "autodirector_camera_fix: failed to patch autodirector view override address={:#x}",
            patch_address
        ));
    }
}

unsafe fn write_conditional_patch(patch_address: usize, disabled_mask: u32) -> bool {
    let Some(trampoline) = (unsafe { allocate_trampoline(patch_address, disabled_mask) }) else {
        return false;
    };

    let mut patch = Vec::with_capacity(AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN);
    emit_absolute_jump(&mut patch, trampoline.as_ptr() as usize);
    patch.resize(AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN, 0x90);

    if unsafe { write_bytes(patch_address, &patch) } {
        TRAMPOLINE_ADDRESS.store(trampoline.as_mut_ptr().cast::<c_void>(), Ordering::SeqCst);
        TRAMPOLINE_LEN.store(trampoline.len(), Ordering::SeqCst);
        log(&format!(
            "autodirector_camera_fix: installed conditional trampoline address={:#x} len={}",
            trampoline.as_ptr() as usize,
            trampoline.len()
        ));
        true
    } else {
        unsafe {
            VirtualFree(trampoline.as_mut_ptr().cast::<c_void>(), 0, MEM_RELEASE);
        }
        false
    }
}

unsafe fn allocate_trampoline(
    patch_address: usize,
    disabled_mask: u32,
) -> Option<&'static mut [u8]> {
    let original_jump_target = unsafe { original_override_jump_target(patch_address) };
    let normal_setup_address = patch_address + AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN;
    let get_observer_state = unsafe { original_call_target(patch_address) };

    let code = build_conditional_trampoline(
        get_observer_state,
        normal_setup_address,
        original_jump_target,
        disabled_mask,
    );

    let memory = unsafe {
        VirtualAlloc(
            null_mut(),
            code.len(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        )
    };
    if memory.is_null() {
        return None;
    }

    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), memory.cast::<u8>(), code.len());
        let mut old_protect = 0u32;
        VirtualProtect(memory, code.len(), PAGE_EXECUTE_READ, &mut old_protect);
        FlushInstructionCache(GetCurrentProcess(), memory, code.len());
        Some(slice::from_raw_parts_mut(memory.cast::<u8>(), code.len()))
    }
}

fn build_conditional_trampoline(
    get_observer_state: usize,
    normal_setup_address: usize,
    original_jump_target: usize,
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
    code.extend_from_slice(&[
        0x48, 0x8b, 0xd6, // mov rdx, rsi
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CameraConfig {
    fixed: bool,
    first_person: bool,
    chase: bool,
    cameraman: bool,
    disabled_mask: u32,
}

impl CameraConfig {
    fn default() -> Self {
        Self {
            fixed: true,
            first_person: false,
            chase: true,
            cameraman: true,
            disabled_mask: DEFAULT_DISABLED_CAMERA_MASK,
        }
    }

    fn refresh_disabled_mask(&mut self) {
        self.disabled_mask = 0;
        if !self.fixed {
            self.disabled_mask |= 1 << AUTODIRECTOR_MODE_FIXED;
        }
        if !self.first_person {
            self.disabled_mask |= 1 << AUTODIRECTOR_MODE_FIRST_PERSON;
        }
        if !self.chase {
            self.disabled_mask |= 1 << AUTODIRECTOR_MODE_CHASE;
        }
        if !self.cameraman {
            self.disabled_mask |= 1 << AUTODIRECTOR_MODE_CAMERAMAN;
        }
    }
}

fn load_config() -> CameraConfig {
    let Some(path) = module_config_path() else {
        log("autodirector_camera_fix: using default config; module path is unavailable");
        return CameraConfig::default();
    };

    let Ok(contents) = std::fs::read_to_string(&path) else {
        log(&format!(
            "autodirector_camera_fix: using default config; missing {}",
            path.display()
        ));
        return CameraConfig::default();
    };

    match parse_config(&contents) {
        Ok(config) => {
            log(&format!(
                "autodirector_camera_fix: loaded config {}",
                path.display()
            ));
            config
        }
        Err(error) => {
            log(&format!(
                "autodirector_camera_fix: invalid config {}; using defaults: {}",
                path.display(),
                error
            ));
            CameraConfig::default()
        }
    }
}

fn parse_config(contents: &str) -> Result<CameraConfig, String> {
    let mut config = CameraConfig::default();
    let mut in_cameras = false;

    for (line_index, raw_line) in contents.lines().enumerate() {
        let line = raw_line
            .split_once('#')
            .map_or(raw_line, |(line, _)| line)
            .trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            in_cameras = line == "[cameras]";
            continue;
        }

        if !in_cameras {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", line_index + 1));
        };
        let key = key.trim();
        let value = parse_bool(value.trim())
            .ok_or_else(|| format!("line {}: expected true or false", line_index + 1))?;

        match key {
            "fixed" | "point_camera" => config.fixed = value,
            "first_person" | "first-person" | "ineye" | "in_eye" => config.first_person = value,
            "chase" => config.chase = value,
            "cameraman" | "freecam" | "free_camera" => config.cameraman = value,
            "top" | "spawn" => {
                log(&format!(
                    "autodirector_camera_fix: config key '{}' is not independently detectable yet; use fixed=false to disable this family",
                    key
                ));
            }
            _ => {
                log(&format!(
                    "autodirector_camera_fix: ignoring unknown config key '{}'",
                    key
                ));
            }
        }
    }

    config.refresh_disabled_mask();
    Ok(config)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
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

fn emit_absolute_jump(code: &mut Vec<u8>, target: usize) {
    code.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
    code.extend_from_slice(&(target as u64).to_le_bytes());
}

unsafe fn original_call_target(patch_address: usize) -> usize {
    let rel32 = unsafe { read_i32(patch_address + 1) } as isize;
    (patch_address + 5).wrapping_add_signed(rel32)
}

unsafe fn original_override_jump_target(patch_address: usize) -> usize {
    let jump_address = patch_address + AUTODIRECTOR_VIEW_OVERRIDE_JMP_OFFSET;
    let rel32 = unsafe { read_i32(jump_address + 1) } as isize;
    (jump_address + AUTODIRECTOR_VIEW_OVERRIDE_JMP_LEN).wrapping_add_signed(rel32)
}

unsafe fn find_patch_address(client: HModule, scan_start: Instant) -> Option<usize> {
    let client = client as usize;
    let text = unsafe { module_text_section(client)? };
    let search = find_pattern(text.data, AUTODIRECTOR_VIEW_OVERRIDE_PATTERN);
    let elapsed = scan_start.elapsed().as_secs_f64() * 1000.0;

    log(&format!(
        "autodirector_camera_fix: scanned client.dll .text size={} matches={} elapsed_ms={elapsed:.3}",
        text.data.len(),
        search.matches
    ));

    let Some(offset) = search.unique_offset else {
        log(&format!(
            "autodirector_camera_fix: expected one signature match in client.dll .text, found {}",
            search.matches
        ));
        return None;
    };

    Some(client + text.rva + offset)
}

struct ModuleSection<'a> {
    rva: usize,
    data: &'a [u8],
}

unsafe fn module_text_section<'a>(module_base: usize) -> Option<ModuleSection<'a>> {
    if unsafe { read_u16(module_base) } != DOS_SIGNATURE {
        return None;
    }

    let pe_offset = unsafe { read_u32(module_base + DOS_LFANEW_OFFSET) } as usize;
    let pe_header = module_base.checked_add(pe_offset)?;
    if unsafe { read_u32(pe_header) } != PE_SIGNATURE {
        return None;
    }

    let section_count = unsafe { read_u16(pe_header + COFF_NUMBER_OF_SECTIONS_OFFSET) } as usize;
    let optional_header_size =
        unsafe { read_u16(pe_header + COFF_SIZE_OF_OPTIONAL_HEADER_OFFSET) } as usize;
    let section_table = pe_header
        .checked_add(PE_HEADERS_SIZE)?
        .checked_add(optional_header_size)?;

    for section_index in 0..section_count {
        let section_header = section_table.checked_add(section_index * SECTION_HEADER_SIZE)?;
        let name = unsafe { slice::from_raw_parts(section_header as *const u8, SECTION_NAME_SIZE) };
        if name != TEXT_SECTION_NAME {
            continue;
        }

        let virtual_size =
            unsafe { read_u32(section_header + SECTION_VIRTUAL_SIZE_OFFSET) } as usize;
        let virtual_address =
            unsafe { read_u32(section_header + SECTION_VIRTUAL_ADDRESS_OFFSET) } as usize;
        let raw_size =
            unsafe { read_u32(section_header + SECTION_SIZE_OF_RAW_DATA_OFFSET) } as usize;
        let size = virtual_size.max(raw_size);
        if size == 0 {
            return None;
        }

        let address = module_base.checked_add(virtual_address)?;
        let data = unsafe { slice::from_raw_parts(address as *const u8, size) };
        return Some(ModuleSection {
            rva: virtual_address,
            data,
        });
    }

    None
}

unsafe fn restore_patch() {
    if !PATCHED.load(Ordering::SeqCst) {
        return;
    }

    let address = PATCH_ADDRESS.load(Ordering::SeqCst) as usize;
    if address == 0 {
        return;
    }

    let mut old_protect = 0u32;
    let ok = unsafe {
        VirtualProtect(
            address as *mut c_void,
            AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN,
            PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    } != 0;
    if !ok {
        log(&format!(
            "autodirector_camera_fix: failed to unprotect for restore address={:#x}",
            address
        ));
        return;
    }

    for (offset, byte) in ORIGINAL_BYTES.iter().enumerate() {
        let byte = byte.load(Ordering::SeqCst);
        unsafe {
            (address as *mut u8).add(offset).write(byte);
        }
    }

    let mut restored_protect = 0u32;
    unsafe {
        FlushInstructionCache(
            GetCurrentProcess(),
            address as *const c_void,
            AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN,
        );
        VirtualProtect(
            address as *mut c_void,
            AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN,
            old_protect,
            &mut restored_protect,
        );
    }

    log(&format!(
        "autodirector_camera_fix: restored autodirector view override address={:#x} len={}",
        address, AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN
    ));

    PATCHED.store(false, Ordering::SeqCst);
    unsafe {
        free_trampoline();
    }
}

unsafe fn free_trampoline() {
    let trampoline = TRAMPOLINE_ADDRESS.swap(null_mut(), Ordering::SeqCst);
    if trampoline.is_null() {
        return;
    }

    TRAMPOLINE_LEN.store(0, Ordering::SeqCst);
    unsafe {
        VirtualFree(trampoline, 0, MEM_RELEASE);
    }
}

unsafe fn write_bytes(address: usize, bytes: &[u8]) -> bool {
    let mut old_protect = 0u32;
    let ok = unsafe {
        VirtualProtect(
            address as *mut c_void,
            bytes.len(),
            PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    } != 0;
    if !ok {
        return false;
    }

    for (offset, byte) in bytes.iter().enumerate() {
        unsafe {
            (address as *mut u8).add(offset).write(*byte);
        }
    }

    let mut restored_protect = 0u32;
    unsafe {
        FlushInstructionCache(GetCurrentProcess(), address as *const c_void, bytes.len());
        VirtualProtect(
            address as *mut c_void,
            bytes.len(),
            old_protect,
            &mut restored_protect,
        );
    }

    true
}

#[derive(Debug, PartialEq, Eq)]
struct PatternSearch {
    matches: usize,
    unique_offset: Option<usize>,
}

fn find_pattern(data: &[u8], pattern: &[Option<u8>]) -> PatternSearch {
    if data.len() < pattern.len() {
        return PatternSearch {
            matches: 0,
            unique_offset: None,
        };
    }

    let mut matches = 0;
    let mut unique_offset = None;

    for (offset, window) in data.windows(pattern.len()).enumerate() {
        if !bytes_match(window, pattern) {
            continue;
        }

        matches += 1;
        unique_offset = if matches == 1 { Some(offset) } else { None };
    }

    PatternSearch {
        matches,
        unique_offset,
    }
}

fn bytes_match(data: &[u8], pattern: &[Option<u8>]) -> bool {
    if data.len() != pattern.len() {
        return false;
    }

    for (offset, expected) in pattern.iter().enumerate() {
        if let Some(byte) = expected
            && data[offset] != *byte
        {
            return false;
        }
    }
    true
}

unsafe fn read_u16(address: usize) -> u16 {
    unsafe { (address as *const u16).read_unaligned() }
}

unsafe fn read_u32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_unaligned() }
}

unsafe fn read_i32(address: usize) -> i32 {
    unsafe { (address as *const i32).read_unaligned() }
}

fn log(message: &str) {
    log_to_game_console(message);

    let Some(path) = module_log_path() else {
        return;
    };
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;
            writeln!(file, "{message}")
        });
}

fn log_to_game_console(message: &str) {
    let Some(message) = CString::new(format!("{message}\n")).ok() else {
        return;
    };

    let tier0 = unsafe { GetModuleHandleA(TIER0_DLL.as_ptr()) };
    if tier0.is_null() {
        return;
    }

    let msg = unsafe { GetProcAddress(tier0, TIER0_MSG_EXPORT.as_ptr()) };
    if msg.is_null() {
        return;
    }

    let msg: Tier0Msg = unsafe { std::mem::transmute(msg) };
    unsafe {
        msg(c"%s".as_ptr(), message.as_ptr());
    }
}

fn module_log_path() -> Option<std::path::PathBuf> {
    Some(module_file_path()?.with_file_name("autodirector_camera_fix.log"))
}

fn module_config_path() -> Option<std::path::PathBuf> {
    Some(module_file_path()?.with_file_name("autodirector_fix_config.toml"))
}

fn module_file_path() -> Option<std::path::PathBuf> {
    let module = MODULE_HANDLE.load(Ordering::SeqCst);
    if module.is_null() {
        return None;
    }

    let mut buffer = [0u8; 1024];
    let len = unsafe {
        GetModuleFileNameA(
            module,
            buffer.as_mut_ptr().cast::<c_char>(),
            buffer.len() as u32,
        )
    } as usize;
    if len == 0 || len >= buffer.len() {
        return None;
    }

    let path = CStr::from_bytes_until_nul(&buffer)
        .ok()
        .and_then(|value| value.to_str().ok())
        .map(std::path::PathBuf::from)?;
    Some(path)
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DisableThreadLibraryCalls(module: HModule) -> i32;
    fn CreateThread(
        thread_attributes: *mut c_void,
        stack_size: usize,
        start_address: unsafe extern "system" fn(*mut c_void) -> u32,
        parameter: *mut c_void,
        creation_flags: u32,
        thread_id: *mut u32,
    ) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn Sleep(milliseconds: u32);
    fn GetModuleHandleA(module_name: *const c_char) -> HModule;
    fn GetModuleHandleExA(flags: u32, module_name: *const c_char, module: *mut HModule) -> i32;
    fn FreeLibraryAndExitThread(module: HModule, exit_code: u32);
    fn GetProcAddress(module: HModule, proc_name: *const c_char) -> *mut c_void;
    fn GetModuleFileNameA(module: HModule, filename: *mut c_char, size: u32) -> u32;
    fn VirtualAlloc(
        address: *mut c_void,
        size: usize,
        allocation_type: u32,
        protect: u32,
    ) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
    fn VirtualProtect(
        address: *mut c_void,
        size: usize,
        new_protect: u32,
        old_protect: *mut u32,
    ) -> i32;
    fn FlushInstructionCache(process: Handle, base_address: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> Handle;
}

#[cfg(test)]
mod tests {
    use super::{
        AUTODIRECTOR_MODE_CAMERAMAN, AUTODIRECTOR_MODE_CHASE, AUTODIRECTOR_MODE_FIRST_PERSON,
        CameraConfig, PatternSearch, bytes_match, find_pattern, parse_config,
    };

    #[test]
    fn matches_wildcard_pattern() {
        let pattern = [Some(0xe8), None, None, Some(0x48)];

        assert!(bytes_match(&[0xe8, 0x11, 0x22, 0x48], &pattern));
        assert!(!bytes_match(&[0xe8, 0x11, 0x22, 0x49], &pattern));
    }

    #[test]
    fn finds_unique_pattern_match() {
        let data = [0x90, 0xe8, 0x01, 0x48, 0x90];
        let pattern = [Some(0xe8), None, Some(0x48)];

        assert_eq!(
            find_pattern(&data, &pattern),
            PatternSearch {
                matches: 1,
                unique_offset: Some(1),
            }
        );
    }

    #[test]
    fn rejects_ambiguous_pattern_matches() {
        let data = [0x90, 0xe8, 0x01, 0x48, 0xe8, 0x02, 0x48, 0x90];
        let pattern = [Some(0xe8), None, Some(0x48)];

        assert_eq!(
            find_pattern(&data, &pattern),
            PatternSearch {
                matches: 2,
                unique_offset: None,
            }
        );
    }

    #[test]
    fn parses_camera_config() {
        let config = parse_config(
            r#"
            [cameras]
            first_person = false
            chase = false
            fixed = true
            cameraman = true
            "#,
        )
        .unwrap();

        assert_eq!(
            config,
            CameraConfig {
                fixed: true,
                first_person: false,
                chase: false,
                cameraman: true,
                disabled_mask: (1 << AUTODIRECTOR_MODE_FIRST_PERSON)
                    | (1 << AUTODIRECTOR_MODE_CHASE),
            }
        );
    }

    #[test]
    fn supports_camera_config_aliases() {
        let config = parse_config(
            r#"
            [cameras]
            in_eye = true
            free_camera = false
            "#,
        )
        .unwrap();

        assert_eq!(config.disabled_mask, 1 << AUTODIRECTOR_MODE_CAMERAMAN);
    }
}
