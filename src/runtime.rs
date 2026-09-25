#![allow(non_snake_case)]

use std::ffi::{CStr, c_char, c_void};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::ptr::null_mut;
use std::slice;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::autodirector_signature::{PATCH_LEN, analyze_autodirector_view_override};
use crate::config::{CameraConfig, parse_camera_config};
use crate::trampoline::{build_conditional_trampoline, emit_absolute_jump};

type HModule = *mut c_void;
type Handle = *mut c_void;

const CLIENT_DLL: &CStr = c"client.dll";
const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_READ: u32 = 0x20;
const MEM_COMMIT: u32 = 0x0000_1000;
const MEM_RESERVE: u32 = 0x0000_2000;
const MEM_RELEASE: u32 = 0x0000_8000;

const DOS_SIGNATURE: u16 = 0x5a4d;
const PE_SIGNATURE: u32 = 0x0000_4550;
const DOS_LFANEW_OFFSET: usize = 0x3c;
const COFF_NUMBER_OF_SECTIONS_OFFSET: usize = 0x06;
const COFF_SIZE_OF_OPTIONAL_HEADER_OFFSET: usize = 0x14;
const PE_HEADERS_SIZE: usize = 0x18;
const SECTION_HEADER_SIZE: usize = 0x28;
const SECTION_NAME_SIZE: usize = 8;
const SECTION_VIRTUAL_SIZE_OFFSET: usize = 0x08;
const SECTION_VIRTUAL_ADDRESS_OFFSET: usize = 0x0c;
const SECTION_SIZE_OF_RAW_DATA_OFFSET: usize = 0x10;
const TEXT_SECTION_NAME: &[u8; SECTION_NAME_SIZE] = b".text\0\0\0";

static MODULE_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static PATCH_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static TRAMPOLINE_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static PATCHED: AtomicBool = AtomicBool::new(false);
static ORIGINAL_BYTES: [AtomicU8; PATCH_LEN] = [const { AtomicU8::new(0) }; PATCH_LEN];
static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

struct ModuleSection<'a> {
    rva: usize,
    data: &'a [u8],
}

#[derive(Clone, Copy)]
struct ResolvedViewOverride {
    patch_address: usize,
    get_observer_state: usize,
    original_jump_target: usize,
    view_setup_argument_move: [u8; 3],
}

pub(crate) unsafe fn process_attach(module: HModule) {
    MODULE_HANDLE.store(module, Ordering::SeqCst);
    unsafe {
        DisableThreadLibraryCalls(module);

        // Keep our module loaded until the worker has finished installing the patch.
        let mut worker_module = null_mut();
        let worker_parameter = if GetModuleHandleExA(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            init_thread as *const c_void as *const c_char,
            &mut worker_module,
        ) != 0
        {
            worker_module
        } else {
            null_mut()
        };
        let thread = CreateThread(null_mut(), 0, init_thread, worker_parameter, 0, null_mut());
        if !thread.is_null() {
            CloseHandle(thread);
        }
    }
}

pub(crate) unsafe fn process_detach() {
    unsafe { restore_patch() };
}

unsafe extern "system" fn init_thread(parameter: *mut c_void) -> u32 {
    begin_log();
    log("injected");

    for _ in 0..600 {
        let client = unsafe { GetModuleHandleA(CLIENT_DLL.as_ptr()) };
        if !client.is_null() {
            unsafe { initialize_patch(client) };
            return unsafe { finish_thread(parameter, 0) };
        }
        unsafe { Sleep(100) };
    }

    log("timed out waiting for client.dll");
    unsafe { finish_thread(parameter, 1) }
}

unsafe fn finish_thread(module: HModule, exit_code: u32) -> u32 {
    if module.is_null() {
        exit_code
    } else {
        unsafe { FreeLibraryAndExitThread(module, exit_code) }
    }
}

unsafe fn initialize_patch(client: HModule) {
    let config = load_config();
    log(&format!(
        "config fixed={} first_person={} chase={} cameraman={} disabled_mask={:#x}",
        config.fixed, config.first_person, config.chase, config.cameraman, config.disabled_mask
    ));
    if config.disabled_mask == 0 {
        log("all camera families use the original override; no patch is needed");
        return;
    }

    let scan_started = Instant::now();
    let Some(view_override) = (unsafe { find_view_override(client, scan_started) }) else {
        log("autodirector view override signature was not found or was unsafe");
        return;
    };

    for (offset, destination) in ORIGINAL_BYTES.iter().enumerate() {
        let byte = unsafe {
            (view_override.patch_address as *const u8)
                .add(offset)
                .read()
        };
        destination.store(byte, Ordering::SeqCst);
    }

    if unsafe { install_conditional_patch(view_override, config.disabled_mask) } {
        PATCH_ADDRESS.store(view_override.patch_address as *mut c_void, Ordering::SeqCst);
        PATCHED.store(true, Ordering::SeqCst);
        log(&format!(
            "patched view override address={:#x} len={} disabled_mask={:#x}",
            view_override.patch_address, PATCH_LEN, config.disabled_mask
        ));
    } else {
        log(&format!(
            "failed to write patch at address={:#x}",
            view_override.patch_address
        ));
    }
}

fn load_config() -> CameraConfig {
    let Some(path) =
        module_file_path().map(|path| path.with_file_name("autodirector-fix-config.toml"))
    else {
        log("module path is unavailable; using default config");
        return CameraConfig::default();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        log(&format!(
            "config missing {}; using defaults",
            path.display()
        ));
        return CameraConfig::default();
    };

    match parse_camera_config(&contents) {
        Ok((config, warnings)) => {
            log(&format!("loaded config {}", path.display()));
            for warning in warnings {
                log(&warning);
            }
            config
        }
        Err(error) => {
            log(&format!(
                "invalid config {}; using defaults: {error}",
                path.display()
            ));
            CameraConfig::default()
        }
    }
}

unsafe fn find_view_override(
    client: HModule,
    scan_started: Instant,
) -> Option<ResolvedViewOverride> {
    let module_base = client as usize;
    let text = unsafe { module_text_section(module_base)? };
    let report = analyze_autodirector_view_override(text.data);
    let elapsed_ms = scan_started.elapsed().as_secs_f64() * 1000.0;
    log(&format!(
        "scanned client.dll .text size={} matches={} status={} elapsed_ms={elapsed_ms:.3}",
        text.data.len(),
        report.matches,
        report.status.as_str()
    ));
    if !report.is_compatible() {
        return None;
    }

    let text_base = module_base + text.rva;
    let view_setup_argument_move = report.view_setup_argument_move?;
    log(&format!(
        "resolved view setup move={:02x} {:02x} {:02x}",
        view_setup_argument_move[0], view_setup_argument_move[1], view_setup_argument_move[2]
    ));
    Some(ResolvedViewOverride {
        patch_address: text_base + report.patch_offset?,
        get_observer_state: text_base + report.get_observer_state_offset?,
        original_jump_target: text_base + report.original_jump_target_offset?,
        view_setup_argument_move,
    })
}

unsafe fn install_conditional_patch(
    view_override: ResolvedViewOverride,
    disabled_mask: u32,
) -> bool {
    let normal_setup_address = view_override.patch_address + PATCH_LEN;
    let code = build_conditional_trampoline(
        view_override.get_observer_state,
        normal_setup_address,
        view_override.original_jump_target,
        view_override.view_setup_argument_move,
        disabled_mask,
    );
    let trampoline = unsafe { allocate_executable(&code) };
    let Some(trampoline) = trampoline else {
        log("failed to allocate trampoline");
        return false;
    };

    let mut patch = Vec::with_capacity(PATCH_LEN);
    emit_absolute_jump(&mut patch, trampoline as usize);
    patch.resize(PATCH_LEN, 0x90);
    if !unsafe { write_bytes(view_override.patch_address, &patch) } {
        unsafe { VirtualFree(trampoline, 0, MEM_RELEASE) };
        return false;
    }

    TRAMPOLINE_ADDRESS.store(trampoline, Ordering::SeqCst);
    log(&format!(
        "installed trampoline address={:#x} len={}",
        trampoline as usize,
        code.len()
    ));
    true
}

unsafe fn allocate_executable(code: &[u8]) -> Option<*mut c_void> {
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
    }
    Some(memory)
}

unsafe fn restore_patch() {
    if !PATCHED.swap(false, Ordering::SeqCst) {
        return;
    }
    let address = PATCH_ADDRESS.swap(null_mut(), Ordering::SeqCst) as usize;
    if address == 0 {
        return;
    }
    let mut original = [0u8; PATCH_LEN];
    for (offset, byte) in ORIGINAL_BYTES.iter().enumerate() {
        original[offset] = byte.load(Ordering::SeqCst);
    }

    if unsafe { write_bytes(address, &original) } {
        log(&format!(
            "restored view override address={address:#x} len={PATCH_LEN}"
        ));
        unsafe { free_trampoline() };
    } else {
        log(&format!(
            "failed to restore view override address={address:#x}"
        ));
    }
}

unsafe fn free_trampoline() {
    let trampoline = TRAMPOLINE_ADDRESS.swap(null_mut(), Ordering::SeqCst);
    if !trampoline.is_null() {
        unsafe { VirtualFree(trampoline, 0, MEM_RELEASE) };
    }
}

unsafe fn write_bytes(address: usize, bytes: &[u8]) -> bool {
    let mut old_protect = 0u32;
    if unsafe {
        VirtualProtect(
            address as *mut c_void,
            bytes.len(),
            PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    } == 0
    {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len());
        FlushInstructionCache(GetCurrentProcess(), address as *const c_void, bytes.len());
        let mut ignored = 0u32;
        VirtualProtect(
            address as *mut c_void,
            bytes.len(),
            old_protect,
            &mut ignored,
        );
    }
    true
}

unsafe fn module_text_section<'a>(module_base: usize) -> Option<ModuleSection<'a>> {
    if unsafe { read_u16(module_base) } != DOS_SIGNATURE {
        return None;
    }
    let pe_header =
        module_base.checked_add(unsafe { read_u32(module_base + DOS_LFANEW_OFFSET) } as usize)?;
    if unsafe { read_u32(pe_header) } != PE_SIGNATURE {
        return None;
    }
    let section_count = unsafe { read_u16(pe_header + COFF_NUMBER_OF_SECTIONS_OFFSET) } as usize;
    let optional_header_size =
        unsafe { read_u16(pe_header + COFF_SIZE_OF_OPTIONAL_HEADER_OFFSET) } as usize;
    let section_table = pe_header
        .checked_add(PE_HEADERS_SIZE)?
        .checked_add(optional_header_size)?;

    for index in 0..section_count {
        let header = section_table.checked_add(index * SECTION_HEADER_SIZE)?;
        let name = unsafe { slice::from_raw_parts(header as *const u8, SECTION_NAME_SIZE) };
        if name != TEXT_SECTION_NAME {
            continue;
        }
        let virtual_size = unsafe { read_u32(header + SECTION_VIRTUAL_SIZE_OFFSET) } as usize;
        let raw_size = unsafe { read_u32(header + SECTION_SIZE_OF_RAW_DATA_OFFSET) } as usize;
        let size = virtual_size.max(raw_size);
        let rva = unsafe { read_u32(header + SECTION_VIRTUAL_ADDRESS_OFFSET) } as usize;
        if size == 0 {
            return None;
        }
        let data =
            unsafe { slice::from_raw_parts(module_base.checked_add(rva)? as *const u8, size) };
        return Some(ModuleSection { rva, data });
    }
    None
}

unsafe fn read_u16(address: usize) -> u16 {
    unsafe { (address as *const u16).read_unaligned() }
}

unsafe fn read_u32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_unaligned() }
}

fn begin_log() {
    let directory = module_file_path()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());

    for suffix in 0..100 {
        let path = directory.join(format!("autodirector_fix-{stamp}-{suffix}.log"));
        let Ok(file) = OpenOptions::new().write(true).create_new(true).open(&path) else {
            continue;
        };
        let _ = LOG_FILE.set(Mutex::new(file));
        log(&format!("log file: {}", path.display()));
        return;
    }
}

fn log(message: &str) {
    let Some(file) = LOG_FILE.get() else {
        return;
    };
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "[autodirector-fix] {message}");
        let _ = file.flush();
    }
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
    CStr::from_bytes_until_nul(&buffer)
        .ok()?
        .to_str()
        .ok()
        .map(std::path::PathBuf::from)
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
    fn FreeLibraryAndExitThread(module: HModule, exit_code: u32) -> !;
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
