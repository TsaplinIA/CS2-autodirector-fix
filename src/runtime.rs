#![allow(non_snake_case)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::null_mut;
use std::slice;
use std::sync::OnceLock;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::time::Instant;

use crate::config::{
    CameraConfig, DEFAULT_DIRECTOR_HOLD_MS, DEFAULT_DISABLED_CAMERA_MASK, DirectorConfig,
    SnapshotConfig, parse_config,
};
use crate::pattern::find_pattern;
use crate::trampoline::{build_conditional_trampoline, emit_absolute_jump};

type HModule = *mut c_void;
type Handle = *mut c_void;
type Tier0Msg = unsafe extern "C" fn(format: *const c_char, ...);
type Tier0ColorMsg = unsafe extern "C" fn(color: ConsoleColor, format: *const c_char, ...);
type HltvEventHandler = unsafe extern "system" fn(observer_state: *mut c_void, event: *mut c_void);
type GameEventName = unsafe extern "system" fn(event: *mut c_void) -> *const c_char;
type GameEventGetInt =
    unsafe extern "system" fn(event: *mut c_void, out: *mut i32, key: *const EventKey);
type ChaseTargetSetter = unsafe extern "system" fn(observer_state: *mut c_void, target: i32);
type ObserverSetMode = unsafe extern "system" fn(observer_state: *mut c_void, mode: i32);

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
// The register that holds viewSetup changes between CS2 builds (rsi, r14,
// rbx). The exact three-byte move is validated and replayed by the trampoline.
const AUTODIRECTOR_VIEW_OVERRIDE_PATTERN: &[Option<u8>] = &[
    Some(0xe8), // call FUN_180b0f460
    None,
    None,
    None,
    None,
    None, // REX.W: mov rdx, a general-purpose register
    Some(0x8b),
    None, // ModRM must encode mov rdx,<source>; validated after matching
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
const HLTV_EVENT_HANDLER_PATCH_LEN: usize = 18;
const HLTV_EVENT_HANDLER_PATTERN: &[Option<u8>] = &[
    Some(0x48),
    Some(0x8b),
    Some(0xc4), // mov rax,rsp
    Some(0x55), // push rbp
    Some(0x57), // push rdi
    Some(0x41),
    Some(0x54), // push r12
    Some(0x48),
    Some(0x8d),
    Some(0x68),
    Some(0x88), // lea rbp,[rax-78h]
    Some(0x48),
    Some(0x81),
    Some(0xec),
    Some(0x60),
    Some(0x01),
    Some(0x00),
    Some(0x00), // sub rsp,160h
    Some(0x80),
    Some(0x3d),
    None,
    None,
    None,
    None,
    Some(0x00), // cmp byte ptr [...],0
    Some(0x48),
    Some(0x8b),
    Some(0xfa), // mov rdi,rdx
    Some(0x4c),
    Some(0x8b),
    Some(0xe1), // mov r12,rcx
    Some(0x0f),
    Some(0x84),
    None,
    None,
    None,
    None, // jz
];
const HLTV_EXTRA_EVENTS_JZ_PATCH_LEN: usize = 2;
const HLTV_EXTRA_EVENTS_JZ_OFFSET: usize = 30;
const HLTV_EXTRA_EVENTS_PATTERN: &[Option<u8>] = &[
    Some(0x48),
    Some(0x8d),
    Some(0x0d),
    None,
    None,
    None,
    None, // lea rcx, debug_hltv cvar
    Some(0xe8),
    None,
    None,
    None,
    None, // call cvar getter
    Some(0x48),
    Some(0x85),
    Some(0xc0), // test rax,rax
    Some(0x75),
    Some(0x0b), // jnz has cvar ptr
    Some(0x48),
    Some(0x8b),
    Some(0x05),
    None,
    None,
    None,
    None, // mov rax,[fallback]
    Some(0x48),
    Some(0x8b),
    Some(0x40),
    Some(0x08), // mov rax,[rax+8]
    Some(0x39),
    Some(0x38), // cmp [rax],edi
    Some(0x74),
    Some(0x5a), // jz skip player_death/round registration
    Some(0xc6),
    Some(0x43),
    Some(0x08),
    Some(0x01), // mov byte ptr [rbx+8],1
    Some(0x4c),
    Some(0x8d),
    Some(0x05),
    None,
    None,
    None,
    None, // lea r8,"player_death"
];
const CHASE_TARGET_SETTER_PATTERN: &[Option<u8>] = &[
    Some(0x53), // push rbx
    Some(0x48),
    Some(0x83),
    Some(0xec),
    Some(0x40), // sub rsp,40h
    Some(0x48),
    Some(0x8b),
    Some(0xd9), // mov rbx,rcx
    Some(0x39),
    Some(0x51),
    Some(0x58), // cmp [rcx+58h],edx
    Some(0x0f),
    Some(0x84),
    None,
    None,
    None,
    None, // jz
    Some(0x89),
    Some(0x51),
    Some(0x58), // mov [rcx+58h],edx
    Some(0x8b),
    Some(0xca), // mov ecx,edx
    Some(0x48),
    Some(0x89),
    Some(0x74),
    Some(0x24),
    Some(0x50), // mov [rsp+50h],rsi
    Some(0xe8),
    None,
    None,
    None,
    None, // call player_by_target
];
const ENTITY_SYSTEM_POINTER_PATTERN: &str = "48 89 ? ? ? ? ? 4C 63 ? ? ? ? ? 44 3B ? ? ? ? ? 0F";
const ENTITY_LIST_OFFSET_PATTERN: &str = "48 8D ? ? E8 ? ? ? ? 8D 85";
const ENTITY_HEALTH_OFFSET_PATTERN: &str = "D9 ? ? C7 81 ? ? ? ? 00 00 00 00 48 8D 15";
const ENTITY_LIFE_STATE_OFFSET_PATTERN: &str = "0F B6 81 ? ? ? ? 3B C2";
const ENTITY_TEAM_NUMBER_OFFSET_PATTERN: &str = "44 0F B6 89 ? ? ? ? 41 3B";
const CONTROLLER_PAWN_HANDLE_OFFSET_PATTERN: &str = "0F B6 81 ? ? ? ? 84 C0 75 ? 8B ? ? ? ? ?";
const PLAYER_COLOR_OFFSET_PATTERN: &str = "8B 96 ? ? ? ? EB 05";
const HUD_PHASE_SECONDS_REMAINING_PATTERN: &str = "48 83 EC 28 E8 ? ? ? ? 48 8B D0 48 85 C0 75 05 48 83 C4 28 C3 8B 8A 3C 0F 00 00 48 8B 05 ? ? ? ? 8B 92 38 0F 00 00 2B 48 44 33 C0 03 D1 0F 49 C2 66 0F 6E C0 0F 5B C0 F3 0F 59 05 ? ? ? ?";

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
const ENTITY_IDENTITY_SIZE: usize = 112;
const ENTITY_CHUNK_COUNT: usize = 64;
const NETWORKABLE_ENTITY_CHUNK_COUNT: usize = 32;
const ENTITY_IDENTITIES_PER_CHUNK: usize = 512;
const PLAYER_ARMOR_OFFSET_FROM_GSI: usize = 0x1c7c;
const PLAYER_EYE_ANGLES_OFFSET: usize = 0x3320;
const ENTITY_ABS_VELOCITY_OFFSET: usize = 0x3fc;
const ENTITY_FLAGS_OFFSET: usize = 0x3f8;
const ENTITY_FLAG_ON_GROUND: u32 = 1;
const ENTITY_GAME_SCENE_NODE_OFFSET: usize = 0x330;
const GAME_SCENE_NODE_ORIGIN_OFFSET: usize = 0x80;
const PLAYER_IS_SCOPED_OFFSET: usize = 0x1c50;
const PLAYER_IS_DEFUSING_OFFSET: usize = 0x1c52;
const PLAYER_SHOTS_FIRED_OFFSET: usize = 0x1c64;
const PLAYER_WEAPON_SERVICES_OFFSET: usize = 0x11e0;
const PLAYER_ITEM_SERVICES_OFFSET: usize = 0x11e8;
const ITEM_SERVICES_HAS_DEFUSER_OFFSET: usize = 0x48;
const ITEM_SERVICES_HAS_HELMET_OFFSET: usize = 0x49;
const PLAYER_FLASH_MAX_ALPHA_OFFSET: usize = 0x13fc;
const PLAYER_FLASH_DURATION_OFFSET: usize = 0x1400;
const PLAYER_IS_WALKING_OFFSET: usize = 0x1c30;
const PLAYER_CURRENT_EQUIPMENT_VALUE_OFFSET: usize = 0x1c80;
const CONTROLLER_ACTION_TRACKING_SERVICES_OFFSET: usize = 0x818;
const ACTION_TRACKING_PER_ROUND_STATS_OFFSET: usize = 0x40;
const ACTION_TRACKING_NUM_ROUND_KILLS_OFFSET: usize = 0x128;
const ACTION_TRACKING_NUM_ROUND_KILLS_HEADSHOTS_OFFSET: usize = 0x12c;
const ACTION_TRACKING_TOTAL_ROUND_DAMAGE_DEALT_OFFSET: usize = 0x130;
const ROUND_STATS_SIZE: usize = 0x68;
const ROUND_STATS_KILLS_OFFSET: usize = 0x30;
const ROUND_STATS_DEATHS_OFFSET: usize = 0x34;
const ROUND_STATS_ASSISTS_OFFSET: usize = 0x38;
const ROUND_STATS_DAMAGE_OFFSET: usize = 0x3c;
const ROUND_STATS_EQUIPMENT_VALUE_OFFSET: usize = 0x40;
const ROUND_STATS_HEADSHOT_KILLS_OFFSET: usize = 0x50;
const ROUND_STATS_UTILITY_DAMAGE_OFFSET: usize = 0x5c;
const ROUND_STATS_ENEMIES_FLASHED_OFFSET: usize = 0x60;
const WEAPON_SERVICES_MY_WEAPONS_OFFSET: usize = 0x48;
const WEAPON_SERVICES_ACTIVE_WEAPON_OFFSET: usize = 0x60;
const WEAPON_CLIP1_OFFSET: usize = 0x16d8;
const WEAPON_RESERVE_AMMO_OFFSET: usize = 0x16e0;
const ECON_ENTITY_ATTRIBUTE_MANAGER_OFFSET: usize = 0x1180;
const ATTRIBUTE_CONTAINER_ITEM_OFFSET: usize = 0x50;
const ECON_ITEM_VIEW_ITEM_DEFINITION_INDEX_OFFSET: usize = 0x1ba;
const TEAM_SCORE_OFFSET: usize = 0x630;
const TEAM_NAME_OFFSET: usize = 0x634;
const TEAM_NAME_SIZE: usize = 129;
const GAME_RULES_PROXY_RULES_OFFSET: usize = 0x600;
const GAME_RULES_FREEZE_PERIOD_OFFSET: usize = 0x40;
const GAME_RULES_WARMUP_PERIOD_OFFSET: usize = 0x41;
const GAME_RULES_ROUND_TIME_OFFSET: usize = 0x68;
const GAME_RULES_ROUND_START_TIME_OFFSET: usize = 0x70;
const GAME_RULES_RESTART_ROUND_TIME_OFFSET: usize = 0x74;
const GAME_RULES_GAME_PHASE_OFFSET: usize = 0x84;
const GAME_RULES_TOTAL_ROUNDS_PLAYED_OFFSET: usize = 0x88;
const GAME_RULES_BOMB_PLANTED_OFFSET: usize = 0x8c7;
const PLANTED_C4_BOMB_TICKING_OFFSET: usize = 0x1160;
const PLANTED_C4_BOMB_SITE_OFFSET: usize = 0x1164;
const PLANTED_C4_BLOW_TIME_OFFSET: usize = 0x1190;
const PLANTED_C4_TIMER_LENGTH_OFFSET: usize = 0x1198;
const SCORE_ENTITY_SCAN_LIMIT: usize = ENTITY_CHUNK_COUNT * ENTITY_IDENTITIES_PER_CHUNK;
const SCORE_ENTITY_INDEX_UNKNOWN: usize = usize::MAX;
const GAME_RULES_ENTITY_INDEX_UNKNOWN: usize = usize::MAX;
const PLANTED_C4_ENTITY_INDEX_UNKNOWN: usize = usize::MAX;
const MAX_PLAYER_WEAPONS: usize = 16;
const MAX_VECTOR_WEAPONS: usize = 64;
const MIN_SNAPSHOT_INTERVAL_MS: u64 = 50;
const SNAPSHOT_WORKER_STOP_TIMEOUT_MS: u32 = 1000;

static MODULE_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static PATCH_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static TRAMPOLINE_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static TRAMPOLINE_LEN: AtomicUsize = AtomicUsize::new(0);
static EVENT_HANDLER_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static EVENT_HANDLER_TRAMPOLINE_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static EVENT_HANDLER_TRAMPOLINE_LEN: AtomicUsize = AtomicUsize::new(0);
static EXTRA_EVENTS_JZ_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static CHASE_TARGET_SETTER_ADDRESS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static DISABLED_CAMERA_MASK: AtomicU32 = AtomicU32::new(DEFAULT_DISABLED_CAMERA_MASK);
static DIRECTOR_ENABLED: AtomicBool = AtomicBool::new(false);
static DIRECTOR_HOLD_MS: AtomicU64 = AtomicU64::new(DEFAULT_DIRECTOR_HOLD_MS);
static DIRECTOR_TARGET: AtomicI32 = AtomicI32::new(-1);
static DIRECTOR_HOLD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_ENABLED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_INTERVAL_MS: AtomicU64 = AtomicU64::new(500);
static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_WORKER_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static SCORE_T_ENTITY_INDEX: AtomicUsize = AtomicUsize::new(SCORE_ENTITY_INDEX_UNKNOWN);
static SCORE_CT_ENTITY_INDEX: AtomicUsize = AtomicUsize::new(SCORE_ENTITY_INDEX_UNKNOWN);
static GAME_RULES_ENTITY_INDEX: AtomicUsize = AtomicUsize::new(GAME_RULES_ENTITY_INDEX_UNKNOWN);
static PLANTED_C4_ENTITY_INDEX: AtomicUsize = AtomicUsize::new(PLANTED_C4_ENTITY_INDEX_UNKNOWN);
static PATCHED: AtomicBool = AtomicBool::new(false);
static EVENT_HANDLER_PATCHED: AtomicBool = AtomicBool::new(false);
static EXTRA_EVENTS_PATCHED: AtomicBool = AtomicBool::new(false);
static TIMELINE_SEQ: AtomicU64 = AtomicU64::new(0);
static TIMELINE_START: OnceLock<Instant> = OnceLock::new();
static ENTITY_PROBE: OnceLock<ResolvedEntityProbe> = OnceLock::new();
static HUD_TIMER_PROBE: OnceLock<ResolvedHudTimerProbe> = OnceLock::new();
static TIMELINE_ROUND: AtomicU32 = AtomicU32::new(0);
static LAST_CAMERA_KIND: AtomicU32 = AtomicU32::new(0);
static LAST_CAMERA_MODE: AtomicI32 = AtomicI32::new(-1);
static LAST_CAMERA_TARGET1: AtomicI32 = AtomicI32::new(-1);
static LAST_CAMERA_TARGET2: AtomicI32 = AtomicI32::new(-1);
static LAST_CAMERA_TARGET: AtomicI32 = AtomicI32::new(-1);
static LAST_CAMERA_TIME_MS: AtomicU64 = AtomicU64::new(0);
static ORIGINAL_BYTES: [AtomicU8; AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN] =
    [const { AtomicU8::new(0) }; AUTODIRECTOR_VIEW_OVERRIDE_PATCH_LEN];
static ORIGINAL_EVENT_HANDLER_BYTES: [AtomicU8; HLTV_EVENT_HANDLER_PATCH_LEN] =
    [const { AtomicU8::new(0) }; HLTV_EVENT_HANDLER_PATCH_LEN];
static ORIGINAL_EXTRA_EVENTS_JZ_BYTES: [AtomicU8; HLTV_EXTRA_EVENTS_JZ_PATCH_LEN] =
    [const { AtomicU8::new(0) }; HLTV_EXTRA_EVENTS_JZ_PATCH_LEN];

#[repr(C)]
#[derive(Clone, Copy)]
struct ConsoleColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

#[repr(C)]
struct EventKey {
    hash: u32,
    index: u32,
    name: *const c_char,
}

#[derive(Clone, Copy)]
struct ResolvedEntityProbe {
    entity_system_pointer: usize,
    entity_list_offset: i32,
    health_offset: i32,
    life_state_offset: i32,
    team_number_offset: i32,
    controller_pawn_handle_offset: i32,
    player_color_offset: i32,
}

#[derive(Clone, Copy)]
struct ResolvedHudTimerProbe {
    tick_source_pointer: usize,
    interval_per_tick: usize,
}

#[derive(Clone, Copy)]
struct SnapshotPlayer {
    team: i32,
    health: i32,
    armor: i32,
    has_helmet: Option<bool>,
    has_defuser: Option<bool>,
    life_state: i32,
    position: Option<Vec3>,
    velocity: Option<Vec3>,
    on_ground: Option<bool>,
    eye_angles: Option<Vec3>,
    weapon_def_index: Option<u16>,
    inventory: [Option<u16>; MAX_PLAYER_WEAPONS],
    has_bomb: bool,
    active_weapon_ammo: Option<i32>,
    total_ammo_left: Option<i32>,
    is_scoped: Option<bool>,
    is_defusing: Option<bool>,
    is_walking: Option<bool>,
    flash_duration: Option<f32>,
    flash_max_alpha: Option<f32>,
    current_equip_value: Option<u16>,
    shots_fired: Option<i32>,
    round_stats: RoundStatsSnapshot,
}

#[derive(Clone, Copy, Default)]
struct RoundStatsSnapshot {
    kills: Option<i32>,
    deaths: Option<i32>,
    assists: Option<i32>,
    damage: Option<i32>,
    equipment_value: Option<i32>,
    headshot_kills: Option<i32>,
    utility_damage: Option<i32>,
    enemies_flashed: Option<i32>,
}

#[derive(Clone, Copy)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Clone, Copy, Default)]
struct SnapshotReadStats {
    chunks: usize,
    entities: usize,
    player_like: usize,
    pawn_resolved: usize,
    rejected_team: usize,
    rejected_health: usize,
    rejected_life: usize,
    rejected_pawn_or_color: usize,
    rejected_pawn_resolve: usize,
}

#[derive(Clone, Copy, Default)]
struct ScoreSnapshot {
    t: Option<i32>,
    ct: Option<i32>,
}

#[derive(Clone, Copy, Default)]
struct GameRulesSnapshot {
    freeze_time: Option<bool>,
    warmup: Option<bool>,
    bomb_planted: Option<bool>,
    round_time_s: Option<i32>,
    round_start_time: Option<f32>,
    restart_round_time: Option<f32>,
    game_phase: Option<i32>,
    total_rounds_played: Option<i32>,
    timer_remaining_s: Option<f32>,
}

impl GameRulesSnapshot {
    fn round_phase(self) -> &'static str {
        if self.warmup == Some(true) {
            "warmup"
        } else if self.bomb_planted == Some(true) {
            "bomb_planted"
        } else if self.freeze_time == Some(true) {
            "freeze"
        } else if self.freeze_time == Some(false) {
            "live"
        } else {
            ""
        }
    }

    fn timer_kind(self) -> &'static str {
        if self.bomb_planted == Some(true) {
            "bomb"
        } else if self.freeze_time == Some(true) {
            "freeze"
        } else if self.freeze_time == Some(false) {
            "round"
        } else {
            ""
        }
    }
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
        stop_snapshot_worker();
    }
    unsafe {
        restore_timeline_hooks();
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
    let (config, director, snapshot) = load_config();
    DISABLED_CAMERA_MASK.store(config.disabled_mask, Ordering::SeqCst);
    DIRECTOR_ENABLED.store(director.enabled, Ordering::SeqCst);
    DIRECTOR_HOLD_MS.store(director.hold_ms, Ordering::SeqCst);
    SNAPSHOT_ENABLED.store(snapshot.enabled, Ordering::SeqCst);
    SNAPSHOT_INTERVAL_MS.store(
        snapshot.interval_ms.max(MIN_SNAPSHOT_INTERVAL_MS),
        Ordering::SeqCst,
    );
    log(&format!(
        "config fixed={} first_person={} chase={} cameraman={} disabled_mask={:#x} director_enabled={} director_hold_ms={} snapshot_enabled={} snapshot_interval_ms={}",
        config.fixed,
        config.first_person,
        config.chase,
        config.cameraman,
        config.disabled_mask,
        director.enabled,
        director.hold_ms,
        snapshot.enabled,
        snapshot.interval_ms
    ));

    unsafe {
        install_timeline_hooks(client);
        install_client_director(client, &director);
        start_snapshot_worker(client, &snapshot);
    }

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
    let view_setup_argument_move = unsafe { original_view_setup_argument_move(patch_address) }?;

    let code = build_conditional_trampoline(
        get_observer_state,
        normal_setup_address,
        original_jump_target,
        view_setup_argument_move,
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

fn load_config() -> (CameraConfig, DirectorConfig, SnapshotConfig) {
    let Some(path) = module_config_path() else {
        log("using default config; module path is unavailable");
        return (
            CameraConfig::default(),
            DirectorConfig::default(),
            SnapshotConfig::default(),
        );
    };

    let Ok(contents) = std::fs::read_to_string(&path) else {
        log(&format!("using default config; missing {}", path.display()));
        return (
            CameraConfig::default(),
            DirectorConfig::default(),
            SnapshotConfig::default(),
        );
    };

    match parse_config(&contents) {
        Ok(parsed) => {
            log(&format!("loaded config {}", path.display()));
            for warning in &parsed.warnings {
                log(&format!("{warning}"));
            }
            (parsed.config, parsed.director, parsed.snapshot)
        }
        Err(error) => {
            log(&format!(
                "invalid config {}; using defaults: {}",
                path.display(),
                error
            ));
            (
                CameraConfig::default(),
                DirectorConfig::default(),
                SnapshotConfig::default(),
            )
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

unsafe fn original_view_setup_argument_move(patch_address: usize) -> Option<[u8; 3]> {
    let move_address = patch_address + 5;
    let bytes = unsafe {
        [
            (move_address as *const u8).read(),
            (move_address as *const u8).add(1).read(),
            (move_address as *const u8).add(2).read(),
        ]
    };

    let rex_w_with_optional_source_extension = matches!(bytes[0], 0x48 | 0x49);
    let moves_a_register_into_rdx = bytes[1] == 0x8b && (bytes[2] & 0xf8) == 0xd0;
    (rex_w_with_optional_source_extension && moves_a_register_into_rdx).then_some(bytes)
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

    let patch_address = client + text.rva + offset;
    let Some(view_setup_argument_move) =
        (unsafe { original_view_setup_argument_move(patch_address) })
    else {
        log("matched autodirector signature has an unsupported viewSetup register move");
        return None;
    };
    log(&format!(
        "resolved autodirector view override signature view_setup_move={:02x} {:02x} {:02x}",
        view_setup_argument_move[0], view_setup_argument_move[1], view_setup_argument_move[2]
    ));

    Some(patch_address)
}

unsafe fn find_unique_pattern_address(
    module: HModule,
    pattern: &[Option<u8>],
    label: &str,
) -> Option<usize> {
    let module_base = module as usize;
    let text = unsafe { module_text_section(module_base)? };
    let search = find_pattern(text.data, pattern);

    let Some(offset) = search.unique_offset else {
        log(&format!(
            "{label} signature expected one match, found {}",
            search.matches
        ));
        return None;
    };

    Some(module_base + text.rva + offset)
}

unsafe fn allocate_absolute_trampoline(
    original_address: usize,
    patch_len: usize,
) -> Option<&'static mut [u8]> {
    let mut code = Vec::with_capacity(patch_len + 14);
    let original_bytes = unsafe { slice::from_raw_parts(original_address as *const u8, patch_len) };
    code.extend_from_slice(original_bytes);
    emit_absolute_jump(&mut code, original_address + patch_len);

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

unsafe extern "system" fn hltv_event_handler_detour(
    observer_state: *mut c_void,
    event: *mut c_void,
) {
    let original = EVENT_HANDLER_TRAMPOLINE_ADDRESS.load(Ordering::SeqCst);
    if !original.is_null() {
        let original: HltvEventHandler = unsafe { std::mem::transmute(original) };
        unsafe {
            original(observer_state, event);
        }
    }

    unsafe {
        log_timeline_event(observer_state, event);
    }
}

unsafe fn log_timeline_event(observer_state: *mut c_void, event: *mut c_void) {
    if observer_state.is_null() || event.is_null() {
        return;
    }

    let Some(event_name) = (unsafe { game_event_name(event) }) else {
        return;
    };

    match event_name {
        "hltv_chase" => {
            let mode = unsafe { read_i32(observer_state as usize + 0x38) };
            let target1 = unsafe { game_event_int(event, "target1") };
            let target2 = unsafe { game_event_int(event, "target2") };
            let ineye = unsafe { game_event_int(event, "ineye") };
            let state_primary = unsafe { read_i32(observer_state as usize + 0x58) };
            let state_secondary = unsafe { read_i32(observer_state as usize + 0x60) };
            update_last_camera(1, mode, target1, target2, -1);
            timeline_log(&format!(
                "type=camera camera=chase mode={} target1={} target2={} ineye={} state_primary={} state_secondary={} summary=\"{}\"",
                mode,
                target1,
                target2,
                ineye,
                state_primary,
                state_secondary,
                format_camera_summary(1, mode, target1, target2, -1)
            ));
            unsafe {
                director_reapply_hold(observer_state, "hltv_chase");
            }
        }
        "hltv_fixed" => {
            let mode = unsafe { read_i32(observer_state as usize + 0x38) };
            let target = unsafe { game_event_int(event, "target") };
            let state_target = unsafe { read_i32(observer_state as usize + 0x3c) };
            let posx = unsafe { game_event_int(event, "posx") };
            let posy = unsafe { game_event_int(event, "posy") };
            let posz = unsafe { game_event_int(event, "posz") };
            update_last_camera(2, mode, -1, -1, target);
            timeline_log(&format!(
                "type=camera camera=fixed mode={} target={} state_target={} pos=({},{},{}) summary=\"{}\"",
                mode,
                target,
                state_target,
                posx,
                posy,
                posz,
                format_camera_summary(2, mode, -1, -1, target)
            ));
            unsafe {
                director_reapply_hold(observer_state, "hltv_fixed");
            }
        }
        "hltv_cameraman" => {
            let mode = unsafe { read_i32(observer_state as usize + 0x38) };
            let userid = unsafe { game_event_int(event, "userid") };
            let state_userid = unsafe { read_i32(observer_state as usize + 0x3c) };
            update_last_camera(3, mode, -1, -1, userid);
            timeline_log(&format!(
                "type=camera camera=cameraman mode={} userid={} state_userid={} summary=\"{}\"",
                mode,
                userid,
                state_userid,
                format_camera_summary(3, mode, -1, -1, userid)
            ));
            unsafe {
                director_reapply_hold(observer_state, "hltv_cameraman");
            }
        }
        "player_death" => {
            let victim = unsafe { game_event_int(event, "userid") };
            let attacker = unsafe { game_event_int(event, "attacker") };
            let assister = unsafe { game_event_int(event, "assister") };
            let camera = last_camera_snapshot();
            let camera_has_attacker = camera_contains_player(&camera, attacker);
            let camera_has_victim = camera_contains_player(&camera, victim);
            timeline_log(&format!(
                "type=kill attacker={} victim={} assister={} camera_before=\"{}\" camera_age_ms={} camera_has_attacker={} camera_has_victim={}",
                attacker,
                victim,
                assister,
                camera.summary,
                camera.age_ms,
                camera_has_attacker,
                camera_has_victim
            ));
            unsafe {
                director_lock_attacker(observer_state, attacker);
            }
        }
        "round_start" => {
            let round = TIMELINE_ROUND.fetch_add(1, Ordering::SeqCst) + 1;
            reset_last_camera();
            timeline_log(&format!("type=round event=start round={round}"));
        }
        "round_end" => timeline_log(&format!(
            "type=round event=end round={}",
            TIMELINE_ROUND.load(Ordering::SeqCst)
        )),
        _ => {}
    }
}

unsafe fn director_lock_attacker(observer_state: *mut c_void, attacker: i32) {
    if !DIRECTOR_ENABLED.load(Ordering::SeqCst) || attacker < 0 {
        return;
    }

    let hold_ms = DIRECTOR_HOLD_MS.load(Ordering::SeqCst);
    let until_ms = timeline_elapsed_ms_u64().saturating_add(hold_ms);
    DIRECTOR_TARGET.store(attacker, Ordering::SeqCst);
    DIRECTOR_HOLD_UNTIL_MS.store(until_ms, Ordering::SeqCst);

    unsafe {
        director_apply_chase_target(observer_state, attacker, "player_death");
    }
}

unsafe fn director_reapply_hold(observer_state: *mut c_void, reason: &str) {
    if !DIRECTOR_ENABLED.load(Ordering::SeqCst) {
        return;
    }

    let now_ms = timeline_elapsed_ms_u64();
    if now_ms > DIRECTOR_HOLD_UNTIL_MS.load(Ordering::SeqCst) {
        return;
    }

    let target = DIRECTOR_TARGET.load(Ordering::SeqCst);
    if target < 0 {
        return;
    }

    unsafe {
        director_apply_chase_target(observer_state, target, reason);
    }
}

unsafe fn director_apply_chase_target(observer_state: *mut c_void, target: i32, reason: &str) {
    if observer_state.is_null() {
        return;
    }

    let setter = CHASE_TARGET_SETTER_ADDRESS.load(Ordering::SeqCst);
    if setter.is_null() {
        timeline_log("type=error message=client_director_missing_chase_target_setter");
        return;
    }

    unsafe {
        set_observer_mode(observer_state, 2);
        ((observer_state as usize + 0x3c) as *mut i32).write(-1);
        ((observer_state as usize + 0x60) as *mut i32).write(-1);

        let setter: ChaseTargetSetter = std::mem::transmute(setter);
        setter(observer_state, target);
    }

    update_last_camera(1, 2, target, -1, -1);
    timeline_log(&format!(
        "type=director action=force_chase target={} reason={} hold_until_ms={}",
        target,
        reason,
        DIRECTOR_HOLD_UNTIL_MS.load(Ordering::SeqCst)
    ));
}

unsafe fn set_observer_mode(observer_state: *mut c_void, mode: i32) {
    let vtable = unsafe { *(observer_state as *const *const usize) };
    if vtable.is_null() {
        return;
    }

    let set_mode = unsafe { *vtable.add(0x30 / std::mem::size_of::<usize>()) };
    if set_mode == 0 {
        return;
    }

    let set_mode: ObserverSetMode = unsafe { std::mem::transmute(set_mode) };
    unsafe {
        set_mode(observer_state, mode);
    }
}

struct CameraSnapshot {
    target1: i32,
    target2: i32,
    target: i32,
    age_ms: u64,
    summary: String,
}

fn update_last_camera(kind: u32, mode: i32, target1: i32, target2: i32, target: i32) {
    LAST_CAMERA_KIND.store(kind, Ordering::SeqCst);
    LAST_CAMERA_MODE.store(mode, Ordering::SeqCst);
    LAST_CAMERA_TARGET1.store(target1, Ordering::SeqCst);
    LAST_CAMERA_TARGET2.store(target2, Ordering::SeqCst);
    LAST_CAMERA_TARGET.store(target, Ordering::SeqCst);
    LAST_CAMERA_TIME_MS.store(timeline_elapsed_ms_u64(), Ordering::SeqCst);
}

fn reset_last_camera() {
    LAST_CAMERA_KIND.store(0, Ordering::SeqCst);
    LAST_CAMERA_MODE.store(-1, Ordering::SeqCst);
    LAST_CAMERA_TARGET1.store(-1, Ordering::SeqCst);
    LAST_CAMERA_TARGET2.store(-1, Ordering::SeqCst);
    LAST_CAMERA_TARGET.store(-1, Ordering::SeqCst);
    LAST_CAMERA_TIME_MS.store(0, Ordering::SeqCst);
}

fn last_camera_snapshot() -> CameraSnapshot {
    let kind = LAST_CAMERA_KIND.load(Ordering::SeqCst);
    let mode = LAST_CAMERA_MODE.load(Ordering::SeqCst);
    let target1 = LAST_CAMERA_TARGET1.load(Ordering::SeqCst);
    let target2 = LAST_CAMERA_TARGET2.load(Ordering::SeqCst);
    let target = LAST_CAMERA_TARGET.load(Ordering::SeqCst);
    let camera_time = LAST_CAMERA_TIME_MS.load(Ordering::SeqCst);
    let now = timeline_elapsed_ms_u64();
    let age_ms = if camera_time == 0 {
        u64::MAX
    } else {
        now.saturating_sub(camera_time)
    };

    CameraSnapshot {
        target1,
        target2,
        target,
        age_ms,
        summary: format_camera_summary(kind, mode, target1, target2, target),
    }
}

fn camera_contains_player(camera: &CameraSnapshot, player: i32) -> bool {
    player != -1
        && (camera.target1 == player || camera.target2 == player || camera.target == player)
}

fn format_camera_summary(kind: u32, mode: i32, target1: i32, target2: i32, target: i32) -> String {
    match kind {
        1 => format!("chase mode={mode} primary={target1} secondary={target2}"),
        2 => format!("fixed mode={mode} target={target}"),
        3 => format!("cameraman mode={mode} userid={target}"),
        _ => "none".to_owned(),
    }
}

unsafe fn game_event_name(event: *mut c_void) -> Option<&'static str> {
    let vtable = unsafe { *(event as *const *const usize) };
    if vtable.is_null() {
        return None;
    }

    let name_fn = unsafe { *vtable.add(1) };
    if name_fn == 0 {
        return None;
    }

    let name_fn: GameEventName = unsafe { std::mem::transmute(name_fn) };
    let name = unsafe { name_fn(event) };
    if name.is_null() {
        return None;
    }

    unsafe { CStr::from_ptr(name) }.to_str().ok()
}

unsafe fn game_event_int(event: *mut c_void, key: &str) -> i32 {
    let Some(key_name) = CString::new(key).ok() else {
        return -1;
    };
    let event_key = EventKey {
        hash: event_key_hash(key),
        index: u32::MAX,
        name: key_name.as_ptr(),
    };
    let mut out = [-1i32, 0];

    let vtable = unsafe { *(event as *const *const usize) };
    if vtable.is_null() {
        return -1;
    }

    let get_int = unsafe { *vtable.add(0x78 / std::mem::size_of::<usize>()) };
    if get_int == 0 {
        return -1;
    }

    let get_int: GameEventGetInt = unsafe { std::mem::transmute(get_int) };
    unsafe {
        get_int(event, out.as_mut_ptr(), &event_key);
    }
    out[0]
}

fn event_key_hash(key: &str) -> u32 {
    const M: u32 = 0x5bd1_e995;

    let lower = key
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let mut hash = (lower.len() as u32) ^ 0x3141_5926;
    let mut chunks = lower.chunks_exact(4);

    for chunk in &mut chunks {
        let value = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let mixed = ((value.wrapping_mul(M) >> 24) ^ value.wrapping_mul(M)).wrapping_mul(M);
        hash = hash.wrapping_mul(M) ^ mixed;
    }

    let tail = chunks.remainder();
    match tail.len() {
        3 => {
            hash ^= (tail[2] as u32) << 16;
            hash ^= (tail[1] as u32) << 8;
            hash = (hash ^ tail[0] as u32).wrapping_mul(M);
        }
        2 => {
            hash ^= (tail[1] as u32) << 8;
            hash = (hash ^ tail[0] as u32).wrapping_mul(M);
        }
        1 => {
            hash = (hash ^ tail[0] as u32).wrapping_mul(M);
        }
        _ => {}
    }

    let hash = ((hash >> 13) ^ hash).wrapping_mul(M);
    (hash >> 15) ^ hash
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

unsafe fn restore_timeline_hooks() {
    if EVENT_HANDLER_PATCHED.load(Ordering::SeqCst) {
        let address = EVENT_HANDLER_ADDRESS.load(Ordering::SeqCst) as usize;
        if address != 0 {
            let mut bytes = [0u8; HLTV_EVENT_HANDLER_PATCH_LEN];
            for (offset, byte) in ORIGINAL_EVENT_HANDLER_BYTES.iter().enumerate() {
                bytes[offset] = byte.load(Ordering::SeqCst);
            }
            if unsafe { write_bytes(address, &bytes) } {
                timeline_log(&format!(
                    "restored hltv event handler hook address={:#x}",
                    address
                ));
            } else {
                log(&format!(
                    "failed to restore hltv event handler hook address={:#x}",
                    address
                ));
            }
        }
        EVENT_HANDLER_PATCHED.store(false, Ordering::SeqCst);
    }

    unsafe {
        free_event_handler_trampoline();
    }

    if EXTRA_EVENTS_PATCHED.load(Ordering::SeqCst) {
        let address = EXTRA_EVENTS_JZ_ADDRESS.load(Ordering::SeqCst) as usize;
        if address != 0 {
            let mut bytes = [0u8; HLTV_EXTRA_EVENTS_JZ_PATCH_LEN];
            for (offset, byte) in ORIGINAL_EXTRA_EVENTS_JZ_BYTES.iter().enumerate() {
                bytes[offset] = byte.load(Ordering::SeqCst);
            }
            if unsafe { write_bytes(address, &bytes) } {
                log(&format!(
                    "restored hltv player_death registration gate address={:#x}",
                    address
                ));
            } else {
                log(&format!(
                    "failed to restore hltv player_death registration gate address={:#x}",
                    address
                ));
            }
        }
        EXTRA_EVENTS_PATCHED.store(false, Ordering::SeqCst);
    }
}

unsafe fn install_timeline_hooks(client: HModule) {
    let _ = TIMELINE_START.set(Instant::now());
    timeline_log("type=status message=timeline_logger_initializing");

    unsafe {
        install_extra_hltv_event_registration_patch(client);
        install_hltv_event_handler_hook(client);
    }
}

unsafe fn install_client_director(client: HModule, config: &DirectorConfig) {
    if !config.enabled {
        timeline_log("type=status message=client_director_disabled");
        return;
    }

    let Some(address) = (unsafe {
        find_unique_pattern_address(client, CHASE_TARGET_SETTER_PATTERN, "chase target setter")
    }) else {
        log("client director is enabled but chase target setter signature was not found");
        timeline_log("type=error message=client_director_chase_target_setter_signature_not_found");
        DIRECTOR_ENABLED.store(false, Ordering::SeqCst);
        return;
    };

    CHASE_TARGET_SETTER_ADDRESS.store(address as *mut c_void, Ordering::SeqCst);
    log(&format!(
        "client director enabled chase_target_setter={:#x} hold_ms={}",
        address, config.hold_ms
    ));
    timeline_log(&format!(
        "type=status message=client_director_enabled chase_target_setter={:#x} hold_ms={}",
        address, config.hold_ms
    ));
}

unsafe fn start_snapshot_worker(client: HModule, config: &SnapshotConfig) {
    if !config.enabled {
        snapshot_log("type=status message=snapshot_disabled");
        return;
    }

    if SNAPSHOT_WORKER_STARTED.swap(true, Ordering::SeqCst) {
        snapshot_log("type=status message=snapshot_worker_already_started");
        return;
    }

    SNAPSHOT_SEQ.store(0, Ordering::SeqCst);
    GAME_RULES_ENTITY_INDEX.store(GAME_RULES_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
    PLANTED_C4_ENTITY_INDEX.store(PLANTED_C4_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
    reset_snapshot_players_csv();

    snapshot_log(&format!(
        "type=status message=snapshot_worker_starting interval_ms={} output=autodirector_snapshot.csv data_layers=\"map:pending score:pending players:entity_list positions:entity_list eye_angles:pawn weapons:active_entity grenades:not_implemented bomb:not_implemented\"",
        config.interval_ms.max(MIN_SNAPSHOT_INTERVAL_MS)
    ));

    let thread =
        unsafe { CreateThread(null_mut(), 0, snapshot_worker_thread, client, 0, null_mut()) };
    if thread.is_null() {
        SNAPSHOT_WORKER_STARTED.store(false, Ordering::SeqCst);
        SNAPSHOT_ENABLED.store(false, Ordering::SeqCst);
        snapshot_log("type=error message=snapshot_worker_create_thread_failed");
        return;
    }

    SNAPSHOT_WORKER_HANDLE.store(thread, Ordering::SeqCst);
}

fn reset_snapshot_players_csv() {
    let Some(path) = module_snapshot_players_csv_path() else {
        return;
    };

    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => snapshot_log(&format!(
            "type=error message=snapshot_players_csv_reset_failed error={}",
            csv_escape(&error.to_string())
        )),
    }
}

unsafe fn stop_snapshot_worker() {
    SNAPSHOT_ENABLED.store(false, Ordering::SeqCst);

    let handle = SNAPSHOT_WORKER_HANDLE.swap(null_mut(), Ordering::SeqCst);
    if handle.is_null() {
        return;
    }

    unsafe {
        WaitForSingleObject(handle, SNAPSHOT_WORKER_STOP_TIMEOUT_MS);
        CloseHandle(handle);
    }
}

unsafe extern "system" fn snapshot_worker_thread(client: *mut c_void) -> u32 {
    while SNAPSHOT_ENABLED.load(Ordering::SeqCst) {
        unsafe {
            write_snapshot(client);
        }

        let interval_ms = SNAPSHOT_INTERVAL_MS
            .load(Ordering::SeqCst)
            .max(MIN_SNAPSHOT_INTERVAL_MS)
            .min(u32::MAX as u64) as u32;
        unsafe {
            Sleep(interval_ms);
        }
    }

    SNAPSHOT_WORKER_STARTED.store(false, Ordering::SeqCst);
    snapshot_log("type=status message=snapshot_worker_stopped");
    0
}

unsafe fn write_snapshot(client: HModule) {
    let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::SeqCst);
    let (players, read_stats, score, game_rules) = unsafe { read_snapshot_players(client) };
    let alive_t = players
        .iter()
        .filter(|player| player.team == 2 && player.life_state == 0 && player.health > 0)
        .count();
    let alive_ct = players
        .iter()
        .filter(|player| player.team == 3 && player.life_state == 0 && player.health > 0)
        .count();

    write_snapshot_players_csv(seq, alive_t, alive_ct, score, game_rules, &players);

    if players.is_empty() {
        snapshot_log(&format!(
            "type=diagnostic reason=no_snapshot_players chunks={} entities={} player_like={} pawn_resolved={} rejected_team={} rejected_health={} rejected_life={} rejected_pawn_or_color={} rejected_pawn_resolve={}",
            read_stats.chunks,
            read_stats.entities,
            read_stats.player_like,
            read_stats.pawn_resolved,
            read_stats.rejected_team,
            read_stats.rejected_health,
            read_stats.rejected_life,
            read_stats.rejected_pawn_or_color,
            read_stats.rejected_pawn_resolve
        ));
    }
}

fn write_snapshot_players_csv(
    seq: u64,
    alive_t: usize,
    alive_ct: usize,
    score: ScoreSnapshot,
    game_rules: GameRulesSnapshot,
    players: &[SnapshotPlayer],
) {
    let Some(path) = module_snapshot_players_csv_path() else {
        return;
    };

    let needs_header = std::fs::metadata(&path)
        .map(|metadata| metadata.len() == 0)
        .unwrap_or(true);
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;

            if needs_header {
                writeln!(
                    file,
                    "snapshot_seq,score_t,score_ct,round_phase,game_phase,timer_remaining_s,freeze_time,bomb_planted,total_rounds_played,alive_t,alive_ct,team,health,armor,has_helmet,has_defuser,life_state,pos_x,pos_y,pos_z,velocity_x,velocity_y,velocity_z,on_ground,eye_pitch,eye_yaw,weapon_name,inventory,has_bomb,active_weapon_ammo,total_ammo_left,scoped,defusing,is_walking,flash_duration,flash_max_alpha,current_equip_value,shots_fired,kills_this_round,assists_this_round,headshot_kills_this_round,damage_this_round,utility_damage_this_round,enemies_flashed_this_round,equipment_value_this_round"
                )?;
            }

            for player in players {
                let position = CsvVec3(player.position);
                let velocity = CsvVec3(player.velocity);
                let eye_angles = CsvVec3(player.eye_angles);
                let fields = [
                    seq.to_string(),
                    format_csv_optional_i32(score.t),
                    format_csv_optional_i32(score.ct),
                    game_rules.round_phase().to_string(),
                    game_phase_name(game_rules.game_phase).to_string(),
                    format_csv_optional_f32(game_rules.timer_remaining_s),
                    format_csv_optional_bool(game_rules.freeze_time).to_string(),
                    format_csv_optional_bool(game_rules.bomb_planted).to_string(),
                    format_csv_optional_i32(game_rules.total_rounds_played),
                    alive_t.to_string(),
                    alive_ct.to_string(),
                    team_name(player.team).to_string(),
                    player.health.to_string(),
                    player.armor.to_string(),
                    format_csv_optional_bool(player.has_helmet).to_string(),
                    format_csv_optional_bool(player.has_defuser).to_string(),
                    player.life_state.to_string(),
                    position.x(),
                    position.y(),
                    position.z(),
                    velocity.x(),
                    velocity.y(),
                    velocity.z(),
                    format_csv_optional_bool(player.on_ground).to_string(),
                    eye_angles.x(),
                    eye_angles.y(),
                    weapon_name(player.weapon_def_index).to_string(),
                    csv_escape(&inventory_names(&player.inventory)),
                    player.has_bomb.to_string(),
                    format_csv_optional_i32(player.active_weapon_ammo),
                    format_csv_optional_i32(player.total_ammo_left),
                    format_csv_optional_bool(player.is_scoped).to_string(),
                    format_csv_optional_bool(player.is_defusing).to_string(),
                    format_csv_optional_bool(player.is_walking).to_string(),
                    format_csv_optional_f32(player.flash_duration),
                    format_csv_optional_f32(player.flash_max_alpha),
                    format_csv_optional_u16(player.current_equip_value),
                    format_csv_optional_i32(player.shots_fired),
                    format_csv_optional_i32(player.round_stats.kills),
                    format_csv_optional_i32(player.round_stats.assists),
                    format_csv_optional_i32(player.round_stats.headshot_kills),
                    format_csv_optional_i32(player.round_stats.damage),
                    format_csv_optional_i32(player.round_stats.utility_damage),
                    format_csv_optional_i32(player.round_stats.enemies_flashed),
                    format_csv_optional_i32(player.round_stats.equipment_value),
                ];
                writeln!(file, "{}", fields.join(","))?;
            }

            Ok(())
        });

    if result.is_err() {
        snapshot_log("type=error message=snapshot_players_csv_write_failed");
    }
}

unsafe fn read_snapshot_players(
    client: HModule,
) -> (
    Vec<SnapshotPlayer>,
    SnapshotReadStats,
    ScoreSnapshot,
    GameRulesSnapshot,
) {
    let mut stats = SnapshotReadStats::default();
    let Some(resolved) = (unsafe { resolve_entity_probe(client) }) else {
        snapshot_log("type=error message=entity_probe_not_resolved");
        return (
            Vec::new(),
            stats,
            ScoreSnapshot::default(),
            GameRulesSnapshot::default(),
        );
    };

    let Some(entity_system) =
        (unsafe { read_process_value::<usize>(resolved.entity_system_pointer as *const usize) })
    else {
        snapshot_log("type=error message=entity_system_pointer_read_failed");
        return (
            Vec::new(),
            stats,
            ScoreSnapshot::default(),
            GameRulesSnapshot::default(),
        );
    };
    if entity_system == 0 || !is_probably_user_pointer(entity_system) {
        snapshot_log(&format!(
            "type=error message=bad_entity_system_pointer value={:#x}",
            entity_system
        ));
        return (
            Vec::new(),
            stats,
            ScoreSnapshot::default(),
            GameRulesSnapshot::default(),
        );
    }

    let entity_list = entity_system.wrapping_add(resolved.entity_list_offset as usize);
    if !is_probably_user_pointer(entity_list) {
        snapshot_log(&format!(
            "type=error message=bad_entity_list_pointer value={:#x}",
            entity_list
        ));
        return (
            Vec::new(),
            stats,
            ScoreSnapshot::default(),
            GameRulesSnapshot::default(),
        );
    }

    let score = unsafe { read_score_snapshot(entity_list, resolved.team_number_offset as usize) };
    let game_rules = unsafe { read_game_rules_snapshot(client, entity_list) };
    let mut players = Vec::new();
    for chunk_index in 0..NETWORKABLE_ENTITY_CHUNK_COUNT.min(ENTITY_CHUNK_COUNT) {
        let chunk_slot = (entity_list + chunk_index * std::mem::size_of::<usize>()) as *const usize;
        let Some(chunk) = (unsafe { read_process_value::<usize>(chunk_slot) }) else {
            continue;
        };
        if chunk == 0 || !is_probably_user_pointer(chunk) {
            continue;
        }
        stats.chunks += 1;

        for index_in_chunk in 0..ENTITY_IDENTITIES_PER_CHUNK {
            let identity = chunk + index_in_chunk * ENTITY_IDENTITY_SIZE;
            let Some(entity) = (unsafe { read_process_value::<usize>(identity as *const usize) })
            else {
                continue;
            };
            if entity == 0 || !is_probably_user_pointer(entity) {
                continue;
            }
            stats.entities += 1;

            let team = unsafe {
                read_process_value::<u8>(
                    (entity + resolved.team_number_offset as usize) as *const u8,
                )
            };
            let health = unsafe {
                read_process_value::<i32>((entity + resolved.health_offset as usize) as *const i32)
            };
            let life_state = unsafe {
                read_process_value::<u8>(
                    (entity + resolved.life_state_offset as usize) as *const u8,
                )
            };
            let pawn_handle = unsafe {
                read_process_value::<u32>(
                    (entity + resolved.controller_pawn_handle_offset as usize) as *const u32,
                )
            };
            let color = unsafe {
                read_process_value::<i32>(
                    (entity + resolved.player_color_offset as usize) as *const i32,
                )
            };

            if !is_player_like_entity(team, health, life_state, pawn_handle, color) {
                update_player_reject_stats(
                    &mut stats,
                    team,
                    health,
                    life_state,
                    pawn_handle,
                    color,
                );
                continue;
            }
            stats.player_like += 1;

            let Some(handle) = pawn_handle else {
                continue;
            };
            let Some(pawn) = (unsafe { entity_from_handle(entity_list, handle) }) else {
                stats.rejected_pawn_resolve += 1;
                continue;
            };
            stats.pawn_resolved += 1;

            let pawn_health = unsafe {
                read_process_value::<i32>((pawn + resolved.health_offset as usize) as *const i32)
            }
            .unwrap_or(-1);
            let pawn_team = unsafe {
                read_process_value::<u8>((pawn + resolved.team_number_offset as usize) as *const u8)
            }
            .map(i32::from)
            .unwrap_or_else(|| team.map(i32::from).unwrap_or(-1));
            let pawn_life = unsafe {
                read_process_value::<u8>((pawn + resolved.life_state_offset as usize) as *const u8)
            }
            .map(i32::from)
            .unwrap_or_else(|| life_state.map(i32::from).unwrap_or(-1));
            let pawn_armor = unsafe {
                read_process_value::<i32>((pawn + PLAYER_ARMOR_OFFSET_FROM_GSI) as *const i32)
            }
            .unwrap_or(-1);
            let item_services = unsafe { read_player_item_services(pawn) };
            let has_helmet = item_services.and_then(|item_services| unsafe {
                read_bool_field(item_services, ITEM_SERVICES_HAS_HELMET_OFFSET)
            });
            let has_defuser = item_services.and_then(|item_services| unsafe {
                read_bool_field(item_services, ITEM_SERVICES_HAS_DEFUSER_OFFSET)
            });
            let position = unsafe { read_player_position(pawn) };
            let velocity = unsafe { read_player_abs_velocity(pawn) };
            let flags =
                unsafe { read_process_value::<u32>((pawn + ENTITY_FLAGS_OFFSET) as *const u32) };
            let on_ground = flags.map(|flags| flags & ENTITY_FLAG_ON_GROUND != 0);
            let eye_angles = unsafe { read_player_eye_angles(pawn) };
            let weapon_services = unsafe { read_player_weapon_services(pawn) };
            let active_weapon = weapon_services.and_then(|weapon_services| unsafe {
                read_active_weapon(weapon_services, entity_list)
            });
            let weapon_def_index =
                active_weapon.and_then(|weapon| unsafe { read_weapon_definition_index(weapon) });
            let active_weapon_ammo = active_weapon.and_then(|weapon| unsafe {
                read_process_value::<i32>((weapon + WEAPON_CLIP1_OFFSET) as *const i32)
            });
            let total_ammo_left = active_weapon.and_then(|weapon| unsafe {
                read_process_value::<i32>((weapon + WEAPON_RESERVE_AMMO_OFFSET) as *const i32)
            });
            let inventory = weapon_services
                .map_or([None; MAX_PLAYER_WEAPONS], |weapon_services| unsafe {
                    read_player_inventory(weapon_services, entity_list)
                });
            let has_bomb = inventory_contains_weapon(&inventory, 49);
            let is_scoped = unsafe { read_bool_field(pawn, PLAYER_IS_SCOPED_OFFSET) };
            let is_defusing = unsafe { read_bool_field(pawn, PLAYER_IS_DEFUSING_OFFSET) };
            let is_walking = unsafe { read_bool_field(pawn, PLAYER_IS_WALKING_OFFSET) };
            let flash_duration = unsafe {
                read_process_value::<f32>((pawn + PLAYER_FLASH_DURATION_OFFSET) as *const f32)
            };
            let flash_max_alpha = unsafe {
                read_process_value::<f32>((pawn + PLAYER_FLASH_MAX_ALPHA_OFFSET) as *const f32)
            };
            let current_equip_value = unsafe {
                read_process_value::<u16>(
                    (pawn + PLAYER_CURRENT_EQUIPMENT_VALUE_OFFSET) as *const u16,
                )
            };
            let shots_fired = unsafe {
                read_process_value::<i32>((pawn + PLAYER_SHOTS_FIRED_OFFSET) as *const i32)
            };
            let round_stats =
                unsafe { read_player_round_stats(entity, game_rules.total_rounds_played) };

            players.push(SnapshotPlayer {
                team: pawn_team,
                health: pawn_health,
                armor: pawn_armor,
                has_helmet,
                has_defuser,
                life_state: pawn_life,
                position,
                velocity,
                on_ground,
                eye_angles,
                weapon_def_index,
                inventory,
                has_bomb,
                active_weapon_ammo,
                total_ammo_left,
                is_scoped,
                is_defusing,
                is_walking,
                flash_duration,
                flash_max_alpha,
                current_equip_value,
                shots_fired,
                round_stats,
            });

            if players.len() >= 16 {
                return (players, stats, score, game_rules);
            }
        }
    }

    (players, stats, score, game_rules)
}

unsafe fn read_score_snapshot(entity_list: usize, team_number_offset: usize) -> ScoreSnapshot {
    let mut score = ScoreSnapshot::default();

    for (team, cache) in [(2, &SCORE_T_ENTITY_INDEX), (3, &SCORE_CT_ENTITY_INDEX)] {
        let entity_index = cache.load(Ordering::SeqCst);
        if entity_index == SCORE_ENTITY_INDEX_UNKNOWN {
            continue;
        }

        if let Some((found_team, value)) =
            unsafe { read_team_score_entity(entity_list, entity_index, team_number_offset) }
        {
            if found_team == team {
                apply_team_score(&mut score, found_team, value);
                continue;
            }
        }

        cache.store(SCORE_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
    }

    if score.t.is_some() && score.ct.is_some() {
        return score;
    }

    for entity_index in [2usize, 3usize] {
        if let Some((team, value)) =
            unsafe { read_team_score_entity(entity_list, entity_index, team_number_offset) }
        {
            apply_team_score(&mut score, team, value);
            cache_score_entity_index(team, entity_index);
        }
    }

    if score.t.is_some() && score.ct.is_some() {
        return score;
    }

    for entity_index in 0..SCORE_ENTITY_SCAN_LIMIT {
        if let Some((team, value)) =
            unsafe { read_team_score_entity(entity_list, entity_index, team_number_offset) }
        {
            apply_team_score(&mut score, team, value);
            cache_score_entity_index(team, entity_index);
            if score.t.is_some() && score.ct.is_some() {
                break;
            }
        }
    }

    score
}

fn cache_score_entity_index(team: i32, entity_index: usize) {
    match team {
        2 => SCORE_T_ENTITY_INDEX.store(entity_index, Ordering::SeqCst),
        3 => SCORE_CT_ENTITY_INDEX.store(entity_index, Ordering::SeqCst),
        _ => {}
    }
}

unsafe fn read_game_rules_snapshot(client: HModule, entity_list: usize) -> GameRulesSnapshot {
    if let Some(game_rules) = unsafe { cached_game_rules(entity_list) } {
        return unsafe { read_game_rules_fields(client, entity_list, game_rules) };
    }

    for entity_index in 0..SCORE_ENTITY_SCAN_LIMIT {
        let Some(entity) = (unsafe { entity_from_index(entity_list, entity_index) }) else {
            continue;
        };
        let Some(game_rules) = (unsafe {
            read_process_value::<usize>((entity + GAME_RULES_PROXY_RULES_OFFSET) as *const usize)
        }) else {
            continue;
        };
        if game_rules == 0 || !is_probably_user_pointer(game_rules) {
            continue;
        }

        let snapshot = unsafe { read_game_rules_fields(client, entity_list, game_rules) };
        if is_plausible_game_rules_snapshot(snapshot) {
            GAME_RULES_ENTITY_INDEX.store(entity_index, Ordering::SeqCst);
            return snapshot;
        }
    }

    GameRulesSnapshot::default()
}

unsafe fn cached_game_rules(entity_list: usize) -> Option<usize> {
    let entity_index = GAME_RULES_ENTITY_INDEX.load(Ordering::SeqCst);
    if entity_index == GAME_RULES_ENTITY_INDEX_UNKNOWN {
        return None;
    }

    let Some(entity) = (unsafe { entity_from_index(entity_list, entity_index) }) else {
        GAME_RULES_ENTITY_INDEX.store(GAME_RULES_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
        return None;
    };
    let Some(game_rules) = (unsafe {
        read_process_value::<usize>((entity + GAME_RULES_PROXY_RULES_OFFSET) as *const usize)
    }) else {
        GAME_RULES_ENTITY_INDEX.store(GAME_RULES_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
        return None;
    };
    if game_rules == 0 || !is_probably_user_pointer(game_rules) {
        GAME_RULES_ENTITY_INDEX.store(GAME_RULES_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
        return None;
    }

    let snapshot = unsafe {
        read_game_rules_fields(
            MODULE_HANDLE.load(Ordering::SeqCst),
            entity_list,
            game_rules,
        )
    };
    if !is_plausible_game_rules_snapshot(snapshot) {
        GAME_RULES_ENTITY_INDEX.store(GAME_RULES_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
        return None;
    }

    Some(game_rules)
}

unsafe fn read_game_rules_fields(
    client: HModule,
    entity_list: usize,
    game_rules: usize,
) -> GameRulesSnapshot {
    let mut snapshot = GameRulesSnapshot {
        freeze_time: unsafe { read_bool_field(game_rules, GAME_RULES_FREEZE_PERIOD_OFFSET) },
        warmup: unsafe { read_bool_field(game_rules, GAME_RULES_WARMUP_PERIOD_OFFSET) },
        bomb_planted: unsafe { read_bool_field(game_rules, GAME_RULES_BOMB_PLANTED_OFFSET) },
        round_time_s: unsafe {
            read_process_value::<i32>((game_rules + GAME_RULES_ROUND_TIME_OFFSET) as *const i32)
        },
        round_start_time: unsafe {
            read_process_value::<f32>(
                (game_rules + GAME_RULES_ROUND_START_TIME_OFFSET) as *const f32,
            )
        },
        restart_round_time: unsafe {
            read_process_value::<f32>(
                (game_rules + GAME_RULES_RESTART_ROUND_TIME_OFFSET) as *const f32,
            )
        },
        game_phase: unsafe {
            read_process_value::<i32>((game_rules + GAME_RULES_GAME_PHASE_OFFSET) as *const i32)
        },
        total_rounds_played: unsafe {
            read_process_value::<i32>(
                (game_rules + GAME_RULES_TOTAL_ROUNDS_PLAYED_OFFSET) as *const i32,
            )
        },
        timer_remaining_s: None,
    };
    snapshot.timer_remaining_s =
        unsafe { read_phase_timer_remaining(client, entity_list, snapshot) };
    snapshot
}

fn is_plausible_game_rules_snapshot(snapshot: GameRulesSnapshot) -> bool {
    snapshot.freeze_time.is_some()
        && snapshot.bomb_planted.is_some()
        && snapshot
            .round_time_s
            .is_some_and(|value| (0..=3600).contains(&value))
        && snapshot
            .game_phase
            .is_some_and(|value| (0..=10).contains(&value))
        && snapshot
            .total_rounds_played
            .is_some_and(|value| (0..=200).contains(&value))
}

unsafe fn read_phase_timer_remaining(
    client: HModule,
    entity_list: usize,
    game_rules: GameRulesSnapshot,
) -> Option<f32> {
    let curtime = unsafe { read_current_game_time(client) }?;
    let remaining = match game_rules.timer_kind() {
        "bomb" => unsafe { read_planted_c4_blow_time(entity_list, curtime) }? - curtime,
        "freeze" => game_rules.restart_round_time? - curtime,
        "round" => game_rules.round_start_time? + game_rules.round_time_s? as f32 - curtime,
        _ => return None,
    };

    remaining
        .is_finite()
        .then_some(remaining.clamp(0.0, 3600.0))
}

unsafe fn read_current_game_time(client: HModule) -> Option<f32> {
    let probe = unsafe { resolve_hud_timer_probe(client) }?;
    let tick_source =
        unsafe { read_process_value::<usize>(probe.tick_source_pointer as *const usize) }?;
    if tick_source == 0 || !is_probably_user_pointer(tick_source) {
        return None;
    }

    let current_tick = unsafe { read_process_value::<i32>((tick_source + 0x44) as *const i32) }?;
    let interval_per_tick =
        unsafe { read_process_value::<f32>(probe.interval_per_tick as *const f32) }?;
    if current_tick < 0 || !interval_per_tick.is_finite() || interval_per_tick <= 0.0 {
        return None;
    }

    Some(current_tick as f32 * interval_per_tick)
}

unsafe fn resolve_hud_timer_probe(client: HModule) -> Option<ResolvedHudTimerProbe> {
    if let Some(resolved) = HUD_TIMER_PROBE.get() {
        return Some(*resolved);
    }

    let address = unsafe { find_pattern_address_str(client, HUD_PHASE_SECONDS_REMAINING_PATTERN) }?;
    let tick_source_pointer = unsafe { read_rip_relative_target(address + 0x1f) };
    let interval_per_tick = unsafe { read_rip_relative_target(address + 0x3e) };
    snapshot_log(&format!(
        "type=status message=hud_timer_probe function={:#x} tick_source_pointer={:#x} interval_per_tick={:#x}",
        address, tick_source_pointer, interval_per_tick
    ));

    let resolved = ResolvedHudTimerProbe {
        tick_source_pointer,
        interval_per_tick,
    };
    let _ = HUD_TIMER_PROBE.set(resolved);
    Some(resolved)
}

unsafe fn read_rip_relative_target(rel_address: usize) -> usize {
    let rel = unsafe { read_i32(rel_address) } as isize;
    (rel_address as isize + 4 + rel) as usize
}

unsafe fn read_planted_c4_blow_time(entity_list: usize, curtime: f32) -> Option<f32> {
    if let Some(blow_time) = unsafe { cached_planted_c4_blow_time(entity_list, curtime) } {
        return Some(blow_time);
    }

    for entity_index in 0..SCORE_ENTITY_SCAN_LIMIT {
        let Some(entity) = (unsafe { entity_from_index(entity_list, entity_index) }) else {
            continue;
        };
        let Some(blow_time) = (unsafe { read_planted_c4_candidate(entity, curtime) }) else {
            continue;
        };

        PLANTED_C4_ENTITY_INDEX.store(entity_index, Ordering::SeqCst);
        return Some(blow_time);
    }

    None
}

unsafe fn cached_planted_c4_blow_time(entity_list: usize, curtime: f32) -> Option<f32> {
    let entity_index = PLANTED_C4_ENTITY_INDEX.load(Ordering::SeqCst);
    if entity_index == PLANTED_C4_ENTITY_INDEX_UNKNOWN {
        return None;
    }

    let Some(entity) = (unsafe { entity_from_index(entity_list, entity_index) }) else {
        PLANTED_C4_ENTITY_INDEX.store(PLANTED_C4_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
        return None;
    };

    let blow_time = unsafe { read_planted_c4_candidate(entity, curtime) };
    if blow_time.is_none() {
        PLANTED_C4_ENTITY_INDEX.store(PLANTED_C4_ENTITY_INDEX_UNKNOWN, Ordering::SeqCst);
    }
    blow_time
}

unsafe fn read_planted_c4_candidate(entity: usize, curtime: f32) -> Option<f32> {
    let ticking = unsafe { read_bool_field(entity, PLANTED_C4_BOMB_TICKING_OFFSET) }?;
    if !ticking {
        return None;
    }

    let site =
        unsafe { read_process_value::<i32>((entity + PLANTED_C4_BOMB_SITE_OFFSET) as *const i32) }?;
    if !(0..=1).contains(&site) {
        return None;
    }

    let blow_time =
        unsafe { read_process_value::<f32>((entity + PLANTED_C4_BLOW_TIME_OFFSET) as *const f32) }?;
    let timer_length = unsafe {
        read_process_value::<f32>((entity + PLANTED_C4_TIMER_LENGTH_OFFSET) as *const f32)
    }?;
    if !blow_time.is_finite()
        || !timer_length.is_finite()
        || !(1.0..=120.0).contains(&timer_length)
        || blow_time < curtime
        || blow_time > curtime + 120.0
    {
        return None;
    }

    Some(blow_time)
}

unsafe fn read_team_score_entity(
    entity_list: usize,
    entity_index: usize,
    team_number_offset: usize,
) -> Option<(i32, i32)> {
    let entity = unsafe { entity_from_index(entity_list, entity_index) }?;
    let team = unsafe { read_process_value::<u8>((entity + team_number_offset) as *const u8) }
        .map(i32::from)?;
    if !(2..=3).contains(&team) {
        return None;
    }

    let score = unsafe { read_process_value::<i32>((entity + TEAM_SCORE_OFFSET) as *const i32) }?;
    if !(0..=60).contains(&score) {
        return None;
    }

    let team_name = unsafe { read_team_name(entity) }?;
    is_plausible_team_name(&team_name).then_some((team, score))
}

fn apply_team_score(score: &mut ScoreSnapshot, team: i32, value: i32) {
    match team {
        2 => score.t = Some(value),
        3 => score.ct = Some(value),
        _ => {}
    }
}

unsafe fn read_team_name(entity: usize) -> Option<String> {
    let bytes = unsafe {
        read_process_value::<[u8; TEAM_NAME_SIZE]>(
            (entity + TEAM_NAME_OFFSET) as *const [u8; TEAM_NAME_SIZE],
        )
    }?;
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if len == 0 {
        return None;
    }

    let text = std::str::from_utf8(&bytes[..len]).ok()?.trim();
    if text.is_empty()
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return None;
    }

    Some(text.to_owned())
}

fn is_plausible_team_name(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("terrorist")
        || lower.contains("counter")
        || lower.contains("ct")
        || lower.contains("team")
}

fn update_player_reject_stats(
    stats: &mut SnapshotReadStats,
    team: Option<u8>,
    health: Option<i32>,
    life_state: Option<u8>,
    pawn_handle: Option<u32>,
    color: Option<i32>,
) {
    if !team.is_some_and(|value| (2..=3).contains(&value)) {
        stats.rejected_team += 1;
    }
    if !health.is_some_and(|value| (-1..=200).contains(&value)) {
        stats.rejected_health += 1;
    }
    if !life_state.is_some_and(|value| value <= 2) {
        stats.rejected_life += 1;
    }
    let pawn_ok = pawn_handle.is_some_and(|value| value != 0 && value != u32::MAX);
    let color_ok = color.is_some_and(|value| (-1..=12).contains(&value));
    if !pawn_ok && !color_ok {
        stats.rejected_pawn_or_color += 1;
    }
}

unsafe fn read_player_position(pawn: usize) -> Option<Vec3> {
    let Some(scene_node) = (unsafe {
        read_process_value::<usize>((pawn + ENTITY_GAME_SCENE_NODE_OFFSET) as *const usize)
    }) else {
        return None;
    };
    if scene_node == 0 || !is_probably_user_pointer(scene_node) {
        return None;
    }

    unsafe { read_vec3(scene_node + GAME_SCENE_NODE_ORIGIN_OFFSET) }
        .filter(|value| is_valid_world_position(*value))
}

unsafe fn read_player_eye_angles(pawn: usize) -> Option<Vec3> {
    let value = unsafe { read_vec3(pawn + PLAYER_EYE_ANGLES_OFFSET) }?;
    is_valid_eye_angles(value).then_some(value)
}

unsafe fn read_player_abs_velocity(pawn: usize) -> Option<Vec3> {
    let value = unsafe { read_vec3(pawn + ENTITY_ABS_VELOCITY_OFFSET) }?;
    is_valid_velocity(value).then_some(value)
}

unsafe fn read_bool_field(base: usize, offset: usize) -> Option<bool> {
    unsafe { read_process_value::<u8>((base + offset) as *const u8) }.map(|value| value != 0)
}

unsafe fn read_vec3(address: usize) -> Option<Vec3> {
    let value = unsafe { read_process_value::<[f32; 3]>(address as *const [f32; 3]) }?;
    Some(Vec3 {
        x: value[0],
        y: value[1],
        z: value[2],
    })
}

fn is_valid_world_position(value: Vec3) -> bool {
    value.x.is_finite()
        && value.y.is_finite()
        && value.z.is_finite()
        && value.x.abs() < 100_000.0
        && value.y.abs() < 100_000.0
        && value.z.abs() < 100_000.0
        && (value.x.abs() + value.y.abs() + value.z.abs()) > 1.0
}

fn is_valid_eye_angles(value: Vec3) -> bool {
    value.x.is_finite()
        && value.y.is_finite()
        && value.z.is_finite()
        && (-180.0..=180.0).contains(&value.x)
        && (-360.0..=360.0).contains(&value.y)
        && (-180.0..=180.0).contains(&value.z)
}

fn is_valid_velocity(value: Vec3) -> bool {
    value.x.is_finite()
        && value.y.is_finite()
        && value.z.is_finite()
        && value.x.abs() < 10_000.0
        && value.y.abs() < 10_000.0
        && value.z.abs() < 10_000.0
}

unsafe fn resolve_entity_probe(client: HModule) -> Option<ResolvedEntityProbe> {
    if let Some(resolved) = ENTITY_PROBE.get() {
        return Some(*resolved);
    }

    let entity_system_pointer =
        unsafe { find_pattern_abs_str(client, ENTITY_SYSTEM_POINTER_PATTERN, 3, 4) };
    let entity_list_offset_address =
        unsafe { find_pattern_address_str(client, ENTITY_LIST_OFFSET_PATTERN) };
    let health_offset_address =
        unsafe { find_pattern_address_str(client, ENTITY_HEALTH_OFFSET_PATTERN) };
    let life_state_offset_address =
        unsafe { find_pattern_address_str(client, ENTITY_LIFE_STATE_OFFSET_PATTERN) };
    let team_number_offset_address =
        unsafe { find_pattern_address_str(client, ENTITY_TEAM_NUMBER_OFFSET_PATTERN) };
    let controller_pawn_handle_offset_address =
        unsafe { find_pattern_address_str(client, CONTROLLER_PAWN_HANDLE_OFFSET_PATTERN) };
    let player_color_offset_address =
        unsafe { find_pattern_address_str(client, PLAYER_COLOR_OFFSET_PATTERN) };

    snapshot_log(&format!(
        "type=status message=entity_probe_patterns entity_system_ptr={} entity_list={} health={} life={} team={} pawn_handle={} color={}",
        format_optional_address(entity_system_pointer),
        format_optional_address(entity_list_offset_address),
        format_optional_address(health_offset_address),
        format_optional_address(life_state_offset_address),
        format_optional_address(team_number_offset_address),
        format_optional_address(controller_pawn_handle_offset_address),
        format_optional_address(player_color_offset_address)
    ));

    let resolved = ResolvedEntityProbe {
        entity_system_pointer: entity_system_pointer?,
        entity_list_offset: unsafe { read_i8(entity_list_offset_address? + 3) } as i32,
        health_offset: unsafe { read_i32(health_offset_address? + 5) },
        life_state_offset: unsafe { read_i32(life_state_offset_address? + 3) },
        team_number_offset: unsafe { read_i32(team_number_offset_address? + 4) },
        controller_pawn_handle_offset: unsafe {
            read_i32(controller_pawn_handle_offset_address? + 13)
        },
        player_color_offset: unsafe { read_i32(player_color_offset_address? + 2) },
    };

    snapshot_log(&format!(
        "type=status message=entity_probe_offsets entity_list={:#x} health={:#x} life={:#x} team={:#x} pawn_handle={:#x} color={:#x}",
        resolved.entity_list_offset,
        resolved.health_offset,
        resolved.life_state_offset,
        resolved.team_number_offset,
        resolved.controller_pawn_handle_offset,
        resolved.player_color_offset
    ));

    let _ = ENTITY_PROBE.set(resolved);
    Some(resolved)
}

unsafe fn install_extra_hltv_event_registration_patch(client: HModule) {
    if EXTRA_EVENTS_PATCHED.load(Ordering::SeqCst) {
        return;
    }

    let Some(address) = (unsafe {
        find_unique_pattern_address(
            client,
            HLTV_EXTRA_EVENTS_PATTERN,
            "hltv extra event registration",
        )
    }) else {
        timeline_log("type=error message=hltv_extra_event_registration_signature_not_found");
        return;
    };
    let jz_address = address + HLTV_EXTRA_EVENTS_JZ_OFFSET;

    for (offset, original_byte) in ORIGINAL_EXTRA_EVENTS_JZ_BYTES.iter().enumerate() {
        let byte = unsafe { (jz_address as *const u8).add(offset).read() };
        original_byte.store(byte, Ordering::SeqCst);
    }

    if unsafe { write_bytes(jz_address, &[0x90, 0x90]) } {
        EXTRA_EVENTS_JZ_ADDRESS.store(jz_address as *mut c_void, Ordering::SeqCst);
        EXTRA_EVENTS_PATCHED.store(true, Ordering::SeqCst);
        log(&format!(
            "type=status message=enabled_hltv_player_death_events address={:#x}",
            jz_address
        ));
        timeline_log(&format!(
            "type=status message=enabled_hltv_player_death_events address={:#x}",
            jz_address
        ));
    } else {
        log(&format!(
            "failed to enable hltv player_death timeline events address={:#x}",
            jz_address
        ));
        timeline_log(&format!(
            "type=error message=failed_to_enable_hltv_player_death_events address={:#x}",
            jz_address
        ));
    }
}

unsafe fn install_hltv_event_handler_hook(client: HModule) {
    if EVENT_HANDLER_PATCHED.load(Ordering::SeqCst) {
        return;
    }

    let Some(address) = (unsafe {
        find_unique_pattern_address(client, HLTV_EVENT_HANDLER_PATTERN, "hltv event handler")
    }) else {
        timeline_log("type=error message=hltv_event_handler_signature_not_found");
        return;
    };

    let Some(trampoline) =
        (unsafe { allocate_absolute_trampoline(address, HLTV_EVENT_HANDLER_PATCH_LEN) })
    else {
        log(&format!(
            "failed to allocate hltv event handler trampoline address={:#x}",
            address
        ));
        timeline_log(&format!(
            "type=error message=failed_to_allocate_hltv_event_handler_trampoline address={:#x}",
            address
        ));
        return;
    };

    for (offset, original_byte) in ORIGINAL_EVENT_HANDLER_BYTES.iter().enumerate() {
        let byte = unsafe { (address as *const u8).add(offset).read() };
        original_byte.store(byte, Ordering::SeqCst);
    }

    let mut patch = Vec::with_capacity(HLTV_EVENT_HANDLER_PATCH_LEN);
    emit_absolute_jump(&mut patch, hltv_event_handler_detour as usize);
    patch.resize(HLTV_EVENT_HANDLER_PATCH_LEN, 0x90);

    if unsafe { write_bytes(address, &patch) } {
        EVENT_HANDLER_ADDRESS.store(address as *mut c_void, Ordering::SeqCst);
        EVENT_HANDLER_TRAMPOLINE_ADDRESS
            .store(trampoline.as_mut_ptr().cast::<c_void>(), Ordering::SeqCst);
        EVENT_HANDLER_TRAMPOLINE_LEN.store(trampoline.len(), Ordering::SeqCst);
        EVENT_HANDLER_PATCHED.store(true, Ordering::SeqCst);
        timeline_log("type=status message=timeline_logger_installed");
        log(&format!(
            "installed hltv event handler hook address={:#x} len={}",
            address, HLTV_EVENT_HANDLER_PATCH_LEN
        ));
    } else {
        unsafe {
            VirtualFree(trampoline.as_mut_ptr().cast::<c_void>(), 0, MEM_RELEASE);
        }
        log(&format!(
            "failed to install hltv event handler hook address={:#x}",
            address
        ));
        timeline_log(&format!(
            "type=error message=failed_to_install_hltv_event_handler_hook address={:#x}",
            address
        ));
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

unsafe fn free_event_handler_trampoline() {
    let trampoline = EVENT_HANDLER_TRAMPOLINE_ADDRESS.swap(null_mut(), Ordering::SeqCst);
    if trampoline.is_null() {
        return;
    }

    EVENT_HANDLER_TRAMPOLINE_LEN.store(0, Ordering::SeqCst);
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

unsafe fn read_i8(address: usize) -> i8 {
    unsafe { (address as *const i8).read_unaligned() }
}

unsafe fn read_i32(address: usize) -> i32 {
    unsafe { (address as *const i32).read_unaligned() }
}

unsafe fn read_process_value<T: Copy>(address: *const T) -> Option<T> {
    if address.is_null() {
        return None;
    }

    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut bytes_read = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            address.cast(),
            value.as_mut_ptr().cast(),
            std::mem::size_of::<T>(),
            &mut bytes_read,
        )
    };

    if ok == 0 || bytes_read != std::mem::size_of::<T>() {
        None
    } else {
        Some(unsafe { value.assume_init() })
    }
}

fn is_probably_user_pointer(value: usize) -> bool {
    (0x10000..0x0000_8000_0000_0000).contains(&value)
}

unsafe fn entity_from_handle(entity_list: usize, handle: u32) -> Option<usize> {
    let entity_index = (handle & 0x7fff) as usize;
    let entity = unsafe { entity_from_index(entity_list, entity_index) }?;
    let identity = unsafe { entity_identity_from_index(entity_list, entity_index) }?;
    let identity_handle = unsafe { read_process_value::<u32>((identity + 16) as *const u32) }?;
    (identity_handle == handle).then_some(entity)
}

unsafe fn entity_from_index(entity_list: usize, entity_index: usize) -> Option<usize> {
    let identity = unsafe { entity_identity_from_index(entity_list, entity_index) }?;
    unsafe { read_process_value::<usize>(identity as *const usize) }
        .filter(|ptr| *ptr != 0 && is_probably_user_pointer(*ptr))
}

unsafe fn read_player_weapon_services(pawn: usize) -> Option<usize> {
    let weapon_services = unsafe {
        read_process_value::<usize>((pawn + PLAYER_WEAPON_SERVICES_OFFSET) as *const usize)
    }?;
    if weapon_services == 0 || !is_probably_user_pointer(weapon_services) {
        None
    } else {
        Some(weapon_services)
    }
}

unsafe fn read_player_item_services(pawn: usize) -> Option<usize> {
    let item_services = unsafe {
        read_process_value::<usize>((pawn + PLAYER_ITEM_SERVICES_OFFSET) as *const usize)
    }?;
    if item_services == 0 || !is_probably_user_pointer(item_services) {
        None
    } else {
        Some(item_services)
    }
}

unsafe fn read_player_round_stats(
    controller: usize,
    total_rounds_played: Option<i32>,
) -> RoundStatsSnapshot {
    let Some(action_tracking_services) = (unsafe {
        read_process_value::<usize>(
            (controller + CONTROLLER_ACTION_TRACKING_SERVICES_OFFSET) as *const usize,
        )
    }) else {
        return RoundStatsSnapshot::default();
    };
    if action_tracking_services == 0 || !is_probably_user_pointer(action_tracking_services) {
        return RoundStatsSnapshot::default();
    }

    let mut stats = unsafe {
        read_current_round_stats_vector(
            action_tracking_services + ACTION_TRACKING_PER_ROUND_STATS_OFFSET,
            total_rounds_played,
        )
    }
    .unwrap_or_default();

    if let Some(kills) = unsafe {
        read_process_value::<i32>(
            (action_tracking_services + ACTION_TRACKING_NUM_ROUND_KILLS_OFFSET) as *const i32,
        )
    }
    .filter(|value| (0..=20).contains(value))
    {
        stats.kills = Some(kills);
    }

    if let Some(headshot_kills) = unsafe {
        read_process_value::<i32>(
            (action_tracking_services + ACTION_TRACKING_NUM_ROUND_KILLS_HEADSHOTS_OFFSET)
                as *const i32,
        )
    }
    .filter(|value| (0..=20).contains(value))
    {
        stats.headshot_kills = Some(headshot_kills);
    }

    if let Some(damage) = unsafe {
        read_process_value::<f32>(
            (action_tracking_services + ACTION_TRACKING_TOTAL_ROUND_DAMAGE_DEALT_OFFSET)
                as *const f32,
        )
    }
    .filter(|value| value.is_finite() && (0.0..=5000.0).contains(value))
    {
        stats.damage = Some(damage.round() as i32);
    }

    stats
}

unsafe fn read_current_round_stats_vector(
    vector: usize,
    total_rounds_played: Option<i32>,
) -> Option<RoundStatsSnapshot> {
    let layouts = [
        VectorLayout {
            data_offset: 0x00,
            count_offset: 0x08,
        },
        VectorLayout {
            data_offset: 0x08,
            count_offset: 0x10,
        },
        VectorLayout {
            data_offset: 0x10,
            count_offset: 0x18,
        },
        VectorLayout {
            data_offset: 0x18,
            count_offset: 0x20,
        },
    ];

    for layout in layouts {
        let Some(data) =
            (unsafe { read_process_value::<usize>((vector + layout.data_offset) as *const usize) })
        else {
            continue;
        };
        if data == 0 || !is_probably_user_pointer(data) {
            continue;
        }

        let Some(count) =
            (unsafe { read_process_value::<i32>((vector + layout.count_offset) as *const i32) })
        else {
            continue;
        };
        if !(1..=64).contains(&count) {
            continue;
        }

        let preferred_index = total_rounds_played
            .filter(|value| *value >= 0)
            .map(|value| value as usize)
            .unwrap_or(count.saturating_sub(1) as usize)
            .min(count.saturating_sub(1) as usize);
        let candidates = [
            preferred_index,
            count.saturating_sub(1) as usize,
            preferred_index.saturating_sub(1),
            0,
        ];

        for index in candidates {
            if index >= count as usize {
                continue;
            }

            let stats_base = data + index * ROUND_STATS_SIZE;
            let stats = unsafe { read_round_stats_fields(stats_base) };
            if is_plausible_round_stats(stats) {
                return Some(stats);
            }
        }
    }

    None
}

unsafe fn read_round_stats_fields(stats_base: usize) -> RoundStatsSnapshot {
    RoundStatsSnapshot {
        kills: unsafe {
            read_process_value::<i32>((stats_base + ROUND_STATS_KILLS_OFFSET) as *const i32)
        },
        deaths: unsafe {
            read_process_value::<i32>((stats_base + ROUND_STATS_DEATHS_OFFSET) as *const i32)
        },
        assists: unsafe {
            read_process_value::<i32>((stats_base + ROUND_STATS_ASSISTS_OFFSET) as *const i32)
        },
        damage: unsafe {
            read_process_value::<i32>((stats_base + ROUND_STATS_DAMAGE_OFFSET) as *const i32)
        },
        equipment_value: unsafe {
            read_process_value::<i32>(
                (stats_base + ROUND_STATS_EQUIPMENT_VALUE_OFFSET) as *const i32,
            )
        },
        headshot_kills: unsafe {
            read_process_value::<i32>(
                (stats_base + ROUND_STATS_HEADSHOT_KILLS_OFFSET) as *const i32,
            )
        },
        utility_damage: unsafe {
            read_process_value::<i32>(
                (stats_base + ROUND_STATS_UTILITY_DAMAGE_OFFSET) as *const i32,
            )
        },
        enemies_flashed: unsafe {
            read_process_value::<i32>(
                (stats_base + ROUND_STATS_ENEMIES_FLASHED_OFFSET) as *const i32,
            )
        },
    }
}

fn is_plausible_round_stats(stats: RoundStatsSnapshot) -> bool {
    is_optional_i32_in_range(stats.kills, 0, 20)
        && is_optional_i32_in_range(stats.deaths, 0, 10)
        && is_optional_i32_in_range(stats.assists, 0, 20)
        && is_optional_i32_in_range(stats.damage, 0, 5000)
        && is_optional_i32_in_range(stats.equipment_value, 0, 30000)
        && is_optional_i32_in_range(stats.headshot_kills, 0, 20)
        && is_optional_i32_in_range(stats.utility_damage, 0, 5000)
        && is_optional_i32_in_range(stats.enemies_flashed, 0, 20)
}

fn is_optional_i32_in_range(value: Option<i32>, min: i32, max: i32) -> bool {
    value.is_some_and(|value| (min..=max).contains(&value))
}

unsafe fn read_active_weapon(weapon_services: usize, entity_list: usize) -> Option<usize> {
    let active_weapon_handle = unsafe {
        read_process_value::<u32>(
            (weapon_services + WEAPON_SERVICES_ACTIVE_WEAPON_OFFSET) as *const u32,
        )
    }?;
    if active_weapon_handle == 0 || active_weapon_handle == u32::MAX {
        return None;
    }

    unsafe { entity_from_handle(entity_list, active_weapon_handle) }
}

unsafe fn read_player_inventory(
    weapon_services: usize,
    entity_list: usize,
) -> [Option<u16>; MAX_PLAYER_WEAPONS] {
    let vector = weapon_services + WEAPON_SERVICES_MY_WEAPONS_OFFSET;
    let layouts = [
        VectorLayout {
            data_offset: 0x00,
            count_offset: 0x08,
        },
        VectorLayout {
            data_offset: 0x08,
            count_offset: 0x00,
        },
        VectorLayout {
            data_offset: 0x00,
            count_offset: 0x10,
        },
        VectorLayout {
            data_offset: 0x08,
            count_offset: 0x10,
        },
    ];

    let mut best = [None; MAX_PLAYER_WEAPONS];
    let mut best_count = 0usize;
    for layout in layouts {
        let inventory = unsafe { read_player_inventory_with_layout(vector, layout, entity_list) };
        let count = inventory.iter().flatten().count();
        if count > best_count {
            best = inventory;
            best_count = count;
        }
    }

    best
}

#[derive(Clone, Copy)]
struct VectorLayout {
    data_offset: usize,
    count_offset: usize,
}

unsafe fn read_player_inventory_with_layout(
    vector: usize,
    layout: VectorLayout,
    entity_list: usize,
) -> [Option<u16>; MAX_PLAYER_WEAPONS] {
    let Some(data) =
        (unsafe { read_process_value::<usize>((vector + layout.data_offset) as *const usize) })
    else {
        return [None; MAX_PLAYER_WEAPONS];
    };
    if data == 0 || !is_probably_user_pointer(data) {
        return [None; MAX_PLAYER_WEAPONS];
    }

    let Some(count) =
        (unsafe { read_process_value::<i32>((vector + layout.count_offset) as *const i32) })
    else {
        return [None; MAX_PLAYER_WEAPONS];
    };
    if !(0..=MAX_VECTOR_WEAPONS as i32).contains(&count) {
        return [None; MAX_PLAYER_WEAPONS];
    }

    let mut inventory = [None; MAX_PLAYER_WEAPONS];
    for index in 0..count as usize {
        let Some(handle) = (unsafe { read_process_value::<u32>((data + index * 4) as *const u32) })
        else {
            continue;
        };
        if handle == 0 || handle == u32::MAX {
            continue;
        }

        let Some(weapon) = (unsafe { entity_from_handle(entity_list, handle) }) else {
            continue;
        };
        let Some(definition_index) = (unsafe { read_weapon_definition_index(weapon) }) else {
            continue;
        };
        push_unique_inventory_weapon(&mut inventory, definition_index);
    }

    inventory
}

unsafe fn read_weapon_definition_index(weapon: usize) -> Option<u16> {
    let definition_index = unsafe {
        read_process_value::<u16>(
            (weapon
                + ECON_ENTITY_ATTRIBUTE_MANAGER_OFFSET
                + ATTRIBUTE_CONTAINER_ITEM_OFFSET
                + ECON_ITEM_VIEW_ITEM_DEFINITION_INDEX_OFFSET) as *const u16,
        )
    }?;

    is_plausible_weapon_definition_index(definition_index).then_some(definition_index)
}

fn push_unique_inventory_weapon(inventory: &mut [Option<u16>; MAX_PLAYER_WEAPONS], weapon: u16) {
    if inventory.iter().any(|existing| *existing == Some(weapon)) {
        return;
    }

    if let Some(slot) = inventory.iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(weapon);
    }
}

fn is_plausible_weapon_definition_index(value: u16) -> bool {
    (1..=100).contains(&value) || (500..=525).contains(&value)
}

unsafe fn entity_identity_from_index(entity_list: usize, entity_index: usize) -> Option<usize> {
    let chunk_index = entity_index / ENTITY_IDENTITIES_PER_CHUNK;
    if chunk_index >= ENTITY_CHUNK_COUNT {
        return None;
    }

    let index_in_chunk = entity_index % ENTITY_IDENTITIES_PER_CHUNK;
    let chunk_slot = (entity_list + chunk_index * std::mem::size_of::<usize>()) as *const usize;
    let chunk = unsafe { read_process_value::<usize>(chunk_slot) }?;
    if chunk == 0 || !is_probably_user_pointer(chunk) {
        return None;
    }

    Some(chunk + index_in_chunk * ENTITY_IDENTITY_SIZE)
}

fn is_player_like_entity(
    team: Option<u8>,
    health: Option<i32>,
    life_state: Option<u8>,
    pawn_handle: Option<u32>,
    color: Option<i32>,
) -> bool {
    let team_ok = team.is_some_and(|value| (2..=3).contains(&value));
    let health_ok = health.is_some_and(|value| (-1..=200).contains(&value));
    let life_ok = life_state.is_some_and(|value| value <= 2);
    let pawn_ok = pawn_handle.is_some_and(|value| value != 0 && value != u32::MAX);
    let color_ok = color.is_some_and(|value| (-1..=12).contains(&value));

    team_ok && health_ok && life_ok && (pawn_ok || color_ok)
}

unsafe fn find_pattern_abs_str(
    module: HModule,
    pattern: &str,
    offset: usize,
    offset_to_next_instruction: usize,
) -> Option<usize> {
    let address = unsafe { find_pattern_address_str(module, pattern) }?;
    let rel_address = address.checked_add(offset)?;
    let rel = unsafe { (rel_address as *const i32).read_unaligned() } as isize;

    Some(
        (rel_address as isize)
            .checked_add(offset_to_next_instruction as isize)?
            .checked_add(rel)? as usize,
    )
}

unsafe fn find_pattern_address_str(module: HModule, pattern: &str) -> Option<usize> {
    let parsed_pattern = parse_byte_pattern(pattern)?;
    let module_base = module as usize;
    let text = unsafe { module_text_section(module_base)? };
    let search = find_pattern(text.data, &parsed_pattern);
    search
        .unique_offset
        .map(|offset| module_base + text.rva + offset)
}

fn parse_byte_pattern(pattern: &str) -> Option<Vec<Option<u8>>> {
    pattern
        .split_whitespace()
        .map(|token| {
            if token == "?" || token == "??" {
                Some(None)
            } else {
                u8::from_str_radix(token, 16).ok().map(Some)
            }
        })
        .collect()
}

fn format_optional_address(address: Option<usize>) -> String {
    address.map_or_else(|| "missing".to_string(), |value| format!("{value:#x}"))
}

struct CsvVec3(Option<Vec3>);

impl CsvVec3 {
    fn x(&self) -> String {
        self.0
            .map_or_else(String::new, |value| format!("{:.3}", value.x))
    }

    fn y(&self) -> String {
        self.0
            .map_or_else(String::new, |value| format!("{:.3}", value.y))
    }

    fn z(&self) -> String {
        self.0
            .map_or_else(String::new, |value| format!("{:.3}", value.z))
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn team_name(team: i32) -> &'static str {
    match team {
        2 => "T",
        3 => "CT",
        _ => "",
    }
}

fn game_phase_name(game_phase: Option<i32>) -> &'static str {
    match game_phase {
        Some(0) => "warmup",
        Some(1) => "playing_standard",
        Some(2) => "playing_first_half",
        Some(3) => "playing_second_half",
        Some(4) => "halftime",
        Some(5) => "match_ended",
        Some(_) | None => "",
    }
}

fn weapon_name(definition_index: Option<u16>) -> &'static str {
    match definition_index {
        Some(1) => "Desert Eagle",
        Some(2) => "Dual Berettas",
        Some(3) => "Five-SeveN",
        Some(4) => "Glock-18",
        Some(7) => "AK-47",
        Some(8) => "AUG",
        Some(9) => "AWP",
        Some(10) => "FAMAS",
        Some(11) => "G3SG1",
        Some(13) => "Galil AR",
        Some(14) => "M249",
        Some(16) => "M4A4",
        Some(17) => "MAC-10",
        Some(19) => "P90",
        Some(23) => "MP5-SD",
        Some(24) => "UMP-45",
        Some(25) => "XM1014",
        Some(26) => "PP-Bizon",
        Some(27) => "MAG-7",
        Some(28) => "Negev",
        Some(29) => "Sawed-Off",
        Some(30) => "Tec-9",
        Some(31) => "Zeus x27",
        Some(32) => "P2000",
        Some(33) => "MP7",
        Some(34) => "MP9",
        Some(35) => "Nova",
        Some(36) => "P250",
        Some(38) => "SCAR-20",
        Some(39) => "SG 553",
        Some(40) => "SSG 08",
        Some(42) => "Knife",
        Some(43) => "Flashbang",
        Some(44) => "HE Grenade",
        Some(45) => "Smoke Grenade",
        Some(46) => "Molotov",
        Some(47) => "Decoy Grenade",
        Some(48) => "Incendiary Grenade",
        Some(49) => "C4",
        Some(60) => "M4A1-S",
        Some(61) => "USP-S",
        Some(63) => "CZ75-Auto",
        Some(64) => "R8 Revolver",
        Some(500..=525) => "Knife",
        Some(_) | None => "",
    }
}

fn inventory_names(inventory: &[Option<u16>; MAX_PLAYER_WEAPONS]) -> String {
    inventory
        .iter()
        .filter_map(|definition_index| {
            let name = weapon_name(*definition_index);
            (!name.is_empty()).then_some(name)
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn inventory_contains_weapon(inventory: &[Option<u16>; MAX_PLAYER_WEAPONS], weapon: u16) -> bool {
    inventory.iter().any(|item| *item == Some(weapon))
}

fn format_csv_optional_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "",
    }
}

fn format_csv_optional_i32(value: Option<i32>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

fn format_csv_optional_f32(value: Option<f32>) -> String {
    value
        .filter(|value| value.is_finite())
        .map_or_else(String::new, |value| format!("{value:.3}"))
}

fn format_csv_optional_u16(value: Option<u16>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
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

fn timeline_log(message: &str) {
    let seq = TIMELINE_SEQ.fetch_add(1, Ordering::SeqCst);
    let elapsed_ms = timeline_elapsed_ms_f64();
    let round = TIMELINE_ROUND.load(Ordering::SeqCst);
    let message = format!(
        "seq={seq} time_s={:.3} elapsed_ms={elapsed_ms:.3} round={round} {message}",
        elapsed_ms / 1000.0
    );

    let Some(path) = module_timeline_log_path() else {
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

fn snapshot_log(message: &str) {
    let elapsed_ms = timeline_elapsed_ms_f64();
    let message = format!(
        "time_s={:.3} elapsed_ms={elapsed_ms:.3} {message}",
        elapsed_ms / 1000.0
    );

    let Some(path) = module_snapshot_log_path() else {
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

fn timeline_elapsed_ms_f64() -> f64 {
    TIMELINE_START
        .get()
        .map(|start| start.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or_default()
}

fn timeline_elapsed_ms_u64() -> u64 {
    timeline_elapsed_ms_f64().max(0.0) as u64
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

fn module_timeline_log_path() -> Option<std::path::PathBuf> {
    Some(module_file_path()?.with_file_name("autodirector_timeline.log"))
}

fn module_snapshot_log_path() -> Option<std::path::PathBuf> {
    Some(module_file_path()?.with_file_name("autodirector_snapshot.log"))
}

fn module_snapshot_players_csv_path() -> Option<std::path::PathBuf> {
    Some(module_file_path()?.with_file_name("autodirector_snapshot.csv"))
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
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
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
    fn ReadProcessMemory(
        process: Handle,
        base_address: *const c_void,
        buffer: *mut c_void,
        size: usize,
        bytes_read: *mut usize,
    ) -> i32;
}
