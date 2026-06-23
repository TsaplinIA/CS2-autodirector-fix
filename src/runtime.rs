#![allow(non_snake_case)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::null_mut;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

use crate::config::{CameraConfig, DEFAULT_DISABLED_CAMERA_MASK, parse_config};
use crate::pattern::find_pattern;
use crate::trampoline::{build_conditional_trampoline, emit_absolute_jump};

type HModule = *mut c_void;
type Handle = *mut c_void;
type Tier0Msg = unsafe extern "C" fn(format: *const c_char, ...);
type Tier0ColorMsg = unsafe extern "C" fn(color: ConsoleColor, format: *const c_char, ...);

const CLIENT_DLL: &CStr = c"client.dll";
const TIER0_DLL: &CStr = c"tier0.dll";
const TIER0_MSG_EXPORT: &CStr = c"Msg";
const TIER0_CON_COLOR_MSG_EXPORT: &CStr = c"ConColorMsg";
const TIER0_COLOR_MSG_EXPORT: &CStr = c"ColorMsg";
const LOG_PREFIX: &str = "[autodirector-fix]";
const LOG_COLOR: ConsoleColor = ConsoleColor {
    r: 80,
    g: 200,
    b: 120,
    a: 255,
};
const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;

// Inside view.cpp FUN_180b77d20. This block is only reached when
// spec_autodirector is enabled and the local observer state exists. It calls
// observerState->vfunc_0x28(viewSetup), then jumps over the normal camera setup.
const AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN: usize = 26;
const AUTODIRECTOR_VIEW_OVERRIDE_JMP_OFFSET: usize = 21;
const AUTODIRECTOR_VIEW_OVERRIDE_JMP_LEN: usize = 5;
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

#[repr(C)]
#[derive(Clone, Copy)]
struct ConsoleColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

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
    log("init thread started");

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

    log("client.dll wait timed out");
    0
}

unsafe fn initialize_patch(client: HModule) {
    let config = load_config();
    DISABLED_CAMERA_MASK.store(config.disabled_mask, Ordering::SeqCst);
    log(&format!(
        "config fixed={} first_person={} chase={} cameraman={} disabled_mask={:#x}",
        config.fixed, config.first_person, config.chase, config.cameraman, config.disabled_mask
    ));

    if config.disabled_mask == 0 {
        log("all camera modes are enabled; patch is not needed");
        return;
    }

    let scan_start = Instant::now();
    let Some(patch_address) = (unsafe { find_patch_address(client, scan_start) }) else {
        let elapsed = scan_start.elapsed().as_secs_f64() * 1000.0;
        log(&format!("signature scan failed elapsed_ms={elapsed:.3}"));
        log("autodirector view override signature was not found");
        return;
    };
    let scan_elapsed = scan_start.elapsed().as_secs_f64() * 1000.0;
    log(&format!(
        "signature scan completed elapsed_ms={scan_elapsed:.3}"
    ));

    for (offset, original_byte) in ORIGINAL_BYTES.iter().enumerate() {
        let byte = unsafe { (patch_address as *const u8).add(offset).read() };
        original_byte.store(byte, Ordering::SeqCst);
    }

    if unsafe { write_conditional_patch(patch_address, config.disabled_mask) } {
        PATCH_ADDRESS.store(patch_address as *mut c_void, Ordering::SeqCst);
        PATCHED.store(true, Ordering::SeqCst);
        log(&format!(
            "patched autodirector view override client={:#x} address={:#x} len={} disabled_mask={:#x}",
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
            "failed to patch autodirector view override address={:#x}",
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
            "installed conditional trampoline address={:#x} len={}",
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

fn load_config() -> CameraConfig {
    let Some(path) = module_config_path() else {
        log("using default config; module path is unavailable");
        return CameraConfig::default();
    };

    let Ok(contents) = std::fs::read_to_string(&path) else {
        log(&format!("using default config; missing {}", path.display()));
        return CameraConfig::default();
    };

    match parse_config(&contents) {
        Ok(parsed) => {
            log(&format!("loaded config {}", path.display()));
            for warning in &parsed.warnings {
                log(&format!("{warning}"));
            }
            parsed.config
        }
        Err(error) => {
            log(&format!(
                "invalid config {}; using defaults: {}",
                path.display(),
                error
            ));
            CameraConfig::default()
        }
    }
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
        "scanned client.dll .text size={} matches={} elapsed_ms={elapsed:.3}",
        text.data.len(),
        search.matches
    ));

    let Some(offset) = search.unique_offset else {
        log(&format!(
            "expected one signature match in client.dll .text, found {}",
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
            "failed to unprotect for restore address={:#x}",
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
        "restored autodirector view override address={:#x} len={}",
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
    let message = format!("{LOG_PREFIX} {message}");

    log_to_game_console(&message);

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

    let color_msg = unsafe { GetProcAddress(tier0, TIER0_CON_COLOR_MSG_EXPORT.as_ptr()) };
    let color_msg = if color_msg.is_null() {
        unsafe { GetProcAddress(tier0, TIER0_COLOR_MSG_EXPORT.as_ptr()) }
    } else {
        color_msg
    };

    unsafe {
        if color_msg.is_null() {
            let msg: Tier0Msg = std::mem::transmute(msg);
            msg(c"%s".as_ptr(), message.as_ptr());
        } else {
            let color_msg: Tier0ColorMsg = std::mem::transmute(color_msg);
            color_msg(LOG_COLOR, c"%s".as_ptr(), message.as_ptr());
        }
    }
}

fn module_log_path() -> Option<std::path::PathBuf> {
    Some(module_file_path()?.with_file_name("autodirector_fix.log"))
}

fn module_config_path() -> Option<std::path::PathBuf> {
    Some(module_file_path()?.with_file_name("autodirector-fix-config.toml"))
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
