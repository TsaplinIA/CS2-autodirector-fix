#![allow(non_snake_case)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::null_mut;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};
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

static MODULE_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static PATCH_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
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

    if unsafe { write_nops(patch_address, AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN) } {
        PATCH_ADDRESS.store(patch_address as *mut c_void, Ordering::SeqCst);
        PATCHED.store(true, Ordering::SeqCst);
        log(&format!(
            "autodirector_camera_fix: patched autodirector view override client={:#x} address={:#x} len={}",
            client as usize, patch_address, AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN
        ));
    } else {
        log(&format!(
            "autodirector_camera_fix: failed to patch autodirector view override address={:#x}",
            patch_address
        ));
    }
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
    if !PATCHED.swap(false, Ordering::SeqCst) {
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
}

unsafe fn write_nops(address: usize, len: usize) -> bool {
    let mut old_protect = 0u32;
    let ok = unsafe {
        VirtualProtect(
            address as *mut c_void,
            len,
            PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    } != 0;
    if !ok {
        return false;
    }

    for offset in 0..len {
        unsafe {
            (address as *mut u8).add(offset).write(0x90);
        }
    }

    let mut restored_protect = 0u32;
    unsafe {
        FlushInstructionCache(GetCurrentProcess(), address as *const c_void, len);
        VirtualProtect(
            address as *mut c_void,
            len,
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
    Some(path.with_file_name("autodirector_camera_fix.log"))
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
    use super::{PatternSearch, bytes_match, find_pattern};

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
}
