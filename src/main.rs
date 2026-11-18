//! NoWatermark — hides the Windows desktop watermark ("Evaluation copy / Insider Preview").
//!
//! How it works: explorer.exe draws the watermark through
//! `CDesktopWatermark::s_DesktopBuildPaint` in shell32.dll. This tool looks up that
//! function's address from Microsoft's public symbol server (cached per shell32 build),
//! overwrites its first instruction in explorer's memory with `ret`, repaints the
//! desktop and exits. Nothing on disk is modified and nothing stays resident; the
//! patch lasts until explorer restarts, so `--install` registers a logon run entry.
//!
//! Usage:
//!   NoWatermark.exe              interactive menu (install / uninstall)
//!   NoWatermark.exe --install    copy to %LOCALAPPDATA%\NoWatermark, run at logon, patch now
//!   NoWatermark.exe --uninstall  remove the logon entry (restart explorer to bring the watermark back)
//!   NoWatermark.exe --patch      patch the running explorer once
//!   NoWatermark.exe --silent     logon mode: wait for the desktop, patch with retries, exit

#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::fs;
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::thread::sleep;
use std::time::{Duration, Instant};

use pdb::FallibleIterator;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Diagnostics::Debug::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const APP_NAME: &str = "NoWatermark";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Mangled-name prefix of `CDesktopWatermark::s_DesktopBuildPaint` (signature-agnostic).
const TARGET_PREFIX: &str = "?s_DesktopBuildPaint@CDesktopWatermark@@";
const RET: u8 = 0xC3;

type Res<T> = Result<T, String>;

// ---------------------------------------------------------------------------
// small helpers

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn app_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(APP_NAME);
    let _ = fs::create_dir_all(&dir);
    dir
}

fn log(msg: &str) {
    let mut t: SYSTEMTIME = unsafe { zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, msg
    );
    let path = app_dir().join("log.txt");
    // keep the log small
    if fs::metadata(&path).map(|m| m.len() > 256 * 1024).unwrap_or(false) {
        let _ = fs::remove_file(&path);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
    let _ = io::stdout().write_all(line.as_bytes());
}

fn message_box(text: &str, flags: u32) -> i32 {
    let text = wide(text);
    let title = wide(APP_NAME);
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), title.as_ptr(), flags) }
}

// ---------------------------------------------------------------------------
// processes & modules

fn current_session() -> u32 {
    let mut s = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut s) };
    s
}

/// explorer.exe processes in the current session.
fn explorer_pids() -> Vec<u32> {
    let session = current_session();
    let mut out = Vec::new();
    unsafe {
        let snap = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0));
        if snap.0 == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap.0, &mut e);
        while ok != 0 {
            if from_wide(&e.szExeFile).eq_ignore_ascii_case("explorer.exe") {
                let mut s = u32::MAX;
                if ProcessIdToSessionId(e.th32ProcessID, &mut s) != 0 && s == session {
                    out.push(e.th32ProcessID);
                }
            }
            ok = Process32NextW(snap.0, &mut e);
        }
    }
    out
}

/// (base, size) of shell32.dll in the given process.
fn find_shell32(pid: u32) -> Res<(usize, usize)> {
    unsafe {
        let mut snap = INVALID_HANDLE_VALUE;
        for _ in 0..10 {
            snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid);
            if snap != INVALID_HANDLE_VALUE || last_error() != ERROR_BAD_LENGTH {
                break;
            }
            sleep(Duration::from_millis(50));
        }
        if snap == INVALID_HANDLE_VALUE {
            return Err(format!("module snapshot failed (error {})", last_error()));
        }
        let snap = Handle(snap);
        let mut m: MODULEENTRY32W = zeroed();
        m.dwSize = size_of::<MODULEENTRY32W>() as u32;
        let mut ok = Module32FirstW(snap.0, &mut m);
        while ok != 0 {
            if from_wide(&m.szModule).eq_ignore_ascii_case("shell32.dll") {
                return Ok((m.modBaseAddr as usize, m.modBaseSize as usize));
            }
            ok = Module32NextW(snap.0, &mut m);
        }
    }
    Err("shell32.dll not loaded yet".into())
}

fn read_mem(proc_: HANDLE, addr: usize, len: usize) -> Res<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut got = 0usize;
    let ok = unsafe {
        ReadProcessMemory(proc_, addr as *const c_void, buf.as_mut_ptr() as *mut c_void, len, &mut got)
    };
    if ok == 0 || got != len {
        return Err(format!("ReadProcessMemory @{addr:#x} failed (error {})", last_error()));
    }
    Ok(buf)
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Reads the CodeView record of the mapped image and returns (pdb_name, symbol-server id).
fn pdb_identity(proc_: HANDLE, base: usize) -> Res<(String, String)> {
    let dos = read_mem(proc_, base, 0x40)?;
    if &dos[0..2] != b"MZ" {
        return Err("bad DOS header".into());
    }
    let nt = base + u32_at(&dos, 0x3C) as usize;
    let hdr = read_mem(proc_, nt, 24 + 240)?;
    if &hdr[0..4] != b"PE\0\0" || u16_at(&hdr, 24) != 0x20B {
        return Err("not a PE32+ image".into());
    }
    // OptionalHeader64.DataDirectory[IMAGE_DIRECTORY_ENTRY_DEBUG]
    let dd = 24 + 112 + 6 * 8;
    let (dbg_rva, dbg_size) = (u32_at(&hdr, dd) as usize, u32_at(&hdr, dd + 4) as usize);
    if dbg_rva == 0 || dbg_size == 0 {
        return Err("no debug directory".into());
    }
    let dbg = read_mem(proc_, base + dbg_rva, dbg_size)?;
    for e in dbg.chunks_exact(28) {
        if u32_at(e, 12) != 2 {
            continue; // IMAGE_DEBUG_TYPE_CODEVIEW
        }
        let (size, rva) = (u32_at(e, 16) as usize, u32_at(e, 20) as usize);
        let cv = read_mem(proc_, base + rva, size.min(4096))?;
        if cv.len() < 25 || &cv[0..4] != b"RSDS" {
            continue;
        }
        let g = &cv[4..20];
        let mut id = format!(
            "{:08X}{:04X}{:04X}",
            u32_at(g, 0),
            u16_at(g, 4),
            u16_at(g, 6)
        );
        for b in &g[8..16] {
            id.push_str(&format!("{b:02X}"));
        }
        id.push_str(&format!("{:X}", u32_at(&cv, 20)));
        let name_bytes = &cv[24..];
        let len = name_bytes.iter().position(|&c| c == 0).unwrap_or(name_bytes.len());
        let full = String::from_utf8_lossy(&name_bytes[..len]).to_string();
        let name = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
        return Ok((name, id));
    }
    Err("no RSDS CodeView record".into())
}

// ---------------------------------------------------------------------------
// symbol lookup (cached)

fn cache_path() -> PathBuf {
    app_dir().join("symbols.txt")
}

fn cached_rva(key: &str) -> Option<u32> {
    let text = fs::read_to_string(cache_path()).ok()?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| u32::from_str_radix(v.trim(), 16).ok()).flatten()
    })
}

fn store_rva(key: &str, rva: u32) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(cache_path()) {
        let _ = writeln!(f, "{key} {rva:X}");
    }
}

fn download_pdb(name: &str, id: &str, dest: &PathBuf) -> Res<()> {
    let url = format!("https://msdl.microsoft.com/download/symbols/{name}/{id}/{name}");
    log(&format!("downloading {url}"));
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(300))
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut reader = resp.into_reader().take(512 * 1024 * 1024);
    let mut file = fs::File::create(dest).map_err(|e| format!("create {dest:?}: {e}"))?;
    io::copy(&mut reader, &mut file).map_err(|e| format!("download failed: {e}"))?;
    Ok(())
}

fn rva_from_pdb(path: &PathBuf) -> Res<u32> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut pdb = pdb::PDB::open(file).map_err(|e| format!("pdb open: {e}"))?;
    let symbols = pdb.global_symbols().map_err(|e| format!("pdb symbols: {e}"))?;
    let map = pdb.address_map().map_err(|e| format!("pdb address map: {e}"))?;
    let mut it = symbols.iter();
    while let Some(sym) = it.next().map_err(|e| format!("pdb iterate: {e}"))? {
        if let Ok(pdb::SymbolData::Public(p)) = sym.parse() {
            if p.name.to_string().starts_with(TARGET_PREFIX) {
                if let Some(rva) = p.offset.to_rva(&map) {
                    return Ok(rva.0);
                }
            }
        }
    }
    Err("s_DesktopBuildPaint not found in symbols (Windows changed the watermark code)".into())
}

fn resolve_rva(name: &str, id: &str) -> Res<u32> {
    let key = format!("{name}/{id}");
    if let Some(rva) = cached_rva(&key) {
        return Ok(rva);
    }
    let tmp = app_dir().join(format!("{id}.pdb.tmp"));
    let result = download_pdb(name, id, &tmp).and_then(|_| rva_from_pdb(&tmp));
    let _ = fs::remove_file(&tmp);
    let rva = result?;
    log(&format!("resolved {TARGET_PREFIX}... = RVA {rva:#x}"));
    store_rva(&key, rva);
    Ok(rva)
}

// ---------------------------------------------------------------------------
// patching

#[derive(PartialEq)]
enum Outcome {
    Patched,
    AlreadyPatched,
}

fn patch_process(pid: u32) -> Res<Outcome> {
    let (base, size) = find_shell32(pid)?;
    let proc_ = Handle(unsafe {
        OpenProcess(
            PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE | PROCESS_QUERY_INFORMATION,
            0,
            pid,
        )
    });
    if proc_.0.is_null() {
        return Err(format!("OpenProcess failed (error {})", last_error()));
    }
    let (name, id) = pdb_identity(proc_.0, base)?;
    let rva = resolve_rva(&name, &id)? as usize;
    if rva == 0 || rva >= size {
        return Err(format!("RVA {rva:#x} outside shell32 (size {size:#x})"));
    }
    let addr = base + rva;

    // sanity: target must be in an executable, committed page of shell32
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
    let n = unsafe {
        VirtualQueryEx(proc_.0, addr as *const c_void, &mut mbi, size_of::<MEMORY_BASIC_INFORMATION>())
    };
    let exec = PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    if n == 0 || mbi.State != MEM_COMMIT || mbi.Protect & exec == 0 {
        return Err(format!("target {addr:#x} is not executable code"));
    }

    if read_mem(proc_.0, addr, 1)?[0] == RET {
        return Ok(Outcome::AlreadyPatched);
    }

    unsafe {
        let mut old: PAGE_PROTECTION_FLAGS = 0;
        if VirtualProtectEx(proc_.0, addr as *const c_void, 1, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return Err(format!("VirtualProtectEx failed (error {})", last_error()));
        }
        let mut written = 0usize;
        let ok = WriteProcessMemory(proc_.0, addr as *const c_void, &RET as *const u8 as *const c_void, 1, &mut written);
        let err = last_error();
        let mut tmp: PAGE_PROTECTION_FLAGS = 0;
        VirtualProtectEx(proc_.0, addr as *const c_void, 1, old, &mut tmp);
        FlushInstructionCache(proc_.0, addr as *const c_void, 1);
        if ok == 0 || written != 1 {
            return Err(format!("WriteProcessMemory failed (error {err})"));
        }
    }
    if read_mem(proc_.0, addr, 1)?[0] != RET {
        return Err("patch did not stick".into());
    }
    Ok(Outcome::Patched)
}

unsafe extern "system" fn redraw_desktop_cb(hwnd: HWND, _: LPARAM) -> BOOL {
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd, cls.as_mut_ptr(), cls.len() as i32);
    let name = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
    if name == "Progman" || name == "WorkerW" {
        RedrawWindow(
            hwnd,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
    }
    1
}

/// The watermark is rendered once into a cached desktop layer, so after patching we
/// make explorer rebuild it: WM_SETTINGCHANGE(SPI_SETNONCLIENTMETRICS, "WindowMetrics")
/// sent only to the desktop window (not broadcast), then a plain repaint.
fn redraw_desktop() {
    unsafe {
        let cls = wide("Progman");
        let progman = FindWindowW(cls.as_ptr(), null());
        if !progman.is_null() {
            let area = wide("WindowMetrics");
            let mut result = 0usize;
            SendMessageTimeoutW(
                progman,
                WM_SETTINGCHANGE,
                SPI_SETNONCLIENTMETRICS as usize,
                area.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                &mut result,
            );
        }
        EnumWindows(Some(redraw_desktop_cb), 0);
    }
}

/// Patches every explorer in this session. Ok(true) if at least one is patched.
fn patch_all() -> Res<bool> {
    let pids = explorer_pids();
    if pids.is_empty() {
        return Err("explorer.exe is not running".into());
    }
    let mut any = false;
    let mut errors = Vec::new();
    for pid in pids {
        match patch_process(pid) {
            Ok(Outcome::Patched) => {
                log(&format!("explorer {pid}: patched"));
                any = true;
            }
            Ok(Outcome::AlreadyPatched) => {
                log(&format!("explorer {pid}: already patched"));
                any = true;
            }
            Err(e) => {
                log(&format!("explorer {pid}: {e}"));
                errors.push(e);
            }
        }
    }
    if any {
        redraw_desktop();
        Ok(true)
    } else {
        Err(errors.join("; "))
    }
}

fn desktop_ready() -> bool {
    let cls = wide("Progman");
    let hwnd = unsafe { FindWindowW(cls.as_ptr(), null()) };
    !hwnd.is_null() && explorer_pids().iter().any(|&p| find_shell32(p).is_ok())
}

/// Logon mode: wait for the desktop, then patch, retrying (e.g. network not up yet).
fn silent_mode() {
    let start = Instant::now();
    while !desktop_ready() {
        if start.elapsed() > Duration::from_secs(300) {
            log("gave up: desktop never became ready");
            return;
        }
        sleep(Duration::from_secs(1));
    }
    sleep(Duration::from_secs(2));
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        match patch_all() {
            Ok(_) => {
                // the desktop may still be painting its first frames; repaint once more
                sleep(Duration::from_secs(3));
                redraw_desktop();
                return;
            }
            Err(e) if Instant::now() < deadline => {
                log(&format!("retrying in 15s: {e}"));
                sleep(Duration::from_secs(15));
            }
            Err(e) => {
                log(&format!("gave up: {e}"));
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// install / uninstall

fn installed_exe() -> PathBuf {
    app_dir().join(format!("{APP_NAME}.exe"))
}

fn install() -> Res<String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = installed_exe();
    let same = fs::canonicalize(&me).ok() == fs::canonicalize(&target).ok();
    if !same {
        fs::copy(&me, &target).map_err(|e| format!("copy to {target:?} failed: {e}"))?;
    }
    let cmd = wide(&format!("\"{}\" --silent", target.display()));
    let (key, name) = (wide(RUN_KEY), wide(APP_NAME));
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            REG_SZ,
            cmd.as_ptr() as *const c_void,
            (cmd.len() * 2) as u32,
        )
    };
    if rc != 0 {
        return Err(format!("writing the Run registry value failed (error {rc})"));
    }
    log(&format!("installed to {}", target.display()));
    patch_all()?;
    Ok(format!("已安装到 {}\n每次登录会自动去除水印（改完即退出，不常驻）。", target.display()))
}

fn uninstall() -> Res<String> {
    let (key, name) = (wide(RUN_KEY), wide(APP_NAME));
    let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
    if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
        return Err(format!("removing the Run registry value failed (error {rc})"));
    }
    let me = std::env::current_exe().ok().and_then(|p| fs::canonicalize(p).ok());
    if me != fs::canonicalize(installed_exe()).ok() {
        let _ = fs::remove_file(installed_exe());
    }
    log("uninstalled");
    Ok("已卸载，登录时不再自动运行。\n重启资源管理器后水印会恢复。".into())
}

fn restart_explorer() {
    for pid in explorer_pids() {
        let h = Handle(unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) });
        if !h.0.is_null() {
            unsafe { TerminateProcess(h.0, 1) };
        }
    }
    sleep(Duration::from_secs(3));
    if explorer_pids().is_empty() {
        let _ = std::process::Command::new("explorer.exe").spawn();
    }
}

fn interactive() {
    let choice = message_box(
        "去除桌面右下角的 Windows 水印。\n\n\
         是：安装（立即生效，之后每次登录自动去除）\n\
         否：卸载（恢复水印）\n\
         取消：退出",
        MB_YESNOCANCEL | MB_ICONQUESTION,
    );
    match choice {
        IDYES => match install() {
            Ok(m) => message_box(&format!("完成！水印已去除。\n\n{m}"), MB_OK | MB_ICONINFORMATION),
            Err(e) => message_box(&format!("失败：{e}\n\n日志：{}", app_dir().join("log.txt").display()), MB_OK | MB_ICONERROR),
        },
        IDNO => match uninstall() {
            Ok(m) => {
                if message_box(&format!("{m}\n\n现在重启资源管理器吗？"), MB_YESNO | MB_ICONQUESTION) == IDYES {
                    restart_explorer();
                }
                0
            }
            Err(e) => message_box(&format!("失败：{e}"), MB_OK | MB_ICONERROR),
        },
        _ => 0,
    };
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let code = match arg.as_str() {
        "" => {
            interactive();
            0
        }
        "--silent" => {
            silent_mode();
            0
        }
        "--patch" => match patch_all() {
            Ok(_) => 0,
            Err(e) => {
                log(&format!("failed: {e}"));
                1
            }
        },
        "--install" => match install() {
            Ok(m) => {
                log(&m);
                0
            }
            Err(e) => {
                log(&format!("failed: {e}"));
                1
            }
        },
        "--uninstall" => match uninstall() {
            Ok(m) => {
                log(&m);
                0
            }
            Err(e) => {
                log(&format!("failed: {e}"));
                1
            }
        },
        other => {
            log(&format!("unknown argument {other}; use --install / --uninstall / --patch / --silent"));
            2
        }
    };
    std::process::exit(code);
}
