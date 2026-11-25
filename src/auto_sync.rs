use crate::tracker::{parse_antigravity_settings_text, ParsedQuotaClipboard};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration as StdDuration, Instant};
use windows::core::{VARIANT, PWSTR};
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, TreeScope_Descendants, UIA_ControlTypePropertyId,
    UIA_TextControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncSource {
    UiAutomation,
    ScreenOcr,
}

#[derive(Debug, Clone)]
pub struct AutoSyncResult {
    pub parsed: ParsedQuotaClipboard,
    pub source: SyncSource,
    pub manual: bool,
}

#[derive(Debug, Clone)]
pub enum AutoSyncMessage {
    Found(AutoSyncResult),
    NotVisible { manual: bool },
    Error { message: String, manual: bool },
}

enum WorkerCommand {
    Trigger { allow_ocr: bool, manual: bool },
    SetEnabled(bool),
    Shutdown,
}

pub struct AutoSyncWorker {
    cmd_tx: Sender<WorkerCommand>,
    res_rx: Receiver<AutoSyncMessage>,
    handle: Option<JoinHandle<()>>,
}

impl AutoSyncWorker {
    pub fn start(enabled: bool) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
        let (res_tx, res_rx) = mpsc::channel::<AutoSyncMessage>();

        let handle = thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut is_enabled = enabled;
            let mut last_bg_scan = Instant::now() - StdDuration::from_secs(60);
            let mut last_ocr_scan = Instant::now() - StdDuration::from_secs(60);

            loop {
                match cmd_rx.recv_timeout(StdDuration::from_millis(500)) {
                    Ok(WorkerCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(WorkerCommand::SetEnabled(flag)) => {
                        is_enabled = flag;
                    }
                    Ok(WorkerCommand::Trigger { allow_ocr, manual }) => {
                        let msg = scan_open_antigravity(allow_ocr, manual);
                        if allow_ocr {
                            last_ocr_scan = Instant::now();
                        }
                        last_bg_scan = Instant::now();
                        let _ = res_tx.send(msg);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if is_enabled && last_bg_scan.elapsed() >= StdDuration::from_secs(6) {
                            let allow_ocr = last_ocr_scan.elapsed() >= StdDuration::from_secs(10);
                            let msg = scan_open_antigravity(allow_ocr, false);
                            if allow_ocr {
                                last_ocr_scan = Instant::now();
                            }
                            last_bg_scan = Instant::now();
                            if matches!(msg, AutoSyncMessage::Found(_)) {
                                let _ = res_tx.send(msg);
                            }
                        }
                    }
                }
            }

            unsafe {
                CoUninitialize();
            }
        });

        Self {
            cmd_tx,
            res_rx,
            handle: Some(handle),
        }
    }

    pub fn trigger_now(&self) {
        let _ = self.cmd_tx.send(WorkerCommand::Trigger {
            allow_ocr: true,
            manual: true,
        });
    }

    pub fn trigger_silent(&self) {
        let _ = self.cmd_tx.send(WorkerCommand::Trigger {
            allow_ocr: true,
            manual: false,
        });
    }

    pub fn set_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.send(WorkerCommand::SetEnabled(enabled));
    }

    pub fn try_recv(&self) -> Option<AutoSyncMessage> {
        self.res_rx.try_recv().ok()
    }

    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for AutoSyncWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct EnumData {
    self_pid: u32,
    wins: Vec<(HWND, i32)>,
}

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let data = &mut *(lparam.0 as *mut EnumData);
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return BOOL(1);
    }

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == data.self_pid {
        return BOOL(1);
    }

    let Ok(proc_handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return BOOL(1);
    };

    let mut buf = [0u16; 512];
    let mut size = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        proc_handle,
        PROCESS_NAME_FORMAT(0),
        PWSTR(buf.as_mut_ptr()),
        &mut size,
    )
    .is_ok();
    let _ = CloseHandle(proc_handle);

    if !ok || size == 0 {
        return BOOL(1);
    }
    let path = String::from_utf16_lossy(&buf[..size as usize]);
    let fname = path.rsplit('\\').next().unwrap_or(&path).to_ascii_lowercase();
    if fname == "antigravity.exe" {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if w >= 300 && h >= 200 {
                data.wins.push((hwnd, w.saturating_mul(h)));
            }
        }
    }
    BOOL(1)
}

pub fn find_antigravity_windows() -> Vec<HWND> {
    let mut data = EnumData {
        self_pid: std::process::id(),
        wins: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_cb),
            LPARAM(&mut data as *mut EnumData as isize),
        );
    }
    data.wins.sort_by(|a, b| b.1.cmp(&a.1));
    data.wins.into_iter().map(|(h, _)| h).collect()
}

pub fn try_read_via_uia(hwnd: HWND) -> Option<ParsedQuotaClipboard> {
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL).ok()?;
        let root = uia.ElementFromHandle(hwnd).ok()?;
        let prop_val = VARIANT::from(UIA_TextControlTypeId.0);
        let cond = uia
            .CreatePropertyCondition(UIA_ControlTypePropertyId, &prop_val)
            .ok()?;
        let arr = root.FindAll(TreeScope_Descendants, &cond).ok()?;
        let count = arr.Length().ok()?.min(2500);
        if count <= 0 {
            return None;
        }
        let mut text_buf = String::with_capacity(4096);
        for i in 0..count {
            if let Ok(el) = arr.GetElement(i) {
                if let Ok(name) = el.CurrentName() {
                    let s = name.to_string();
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        text_buf.push_str(trimmed);
                        text_buf.push('\n');
                    }
                }
            }
        }
        parse_antigravity_settings_text(&text_buf).ok()
    }
}

pub fn try_read_via_ocr(hwnd: HWND) -> Result<Option<ParsedQuotaClipboard>, String> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect).map_err(|e| e.to_string())?;
    }
    let width = (rect.right - rect.left).clamp(1, 4096);
    let height = (rect.bottom - rect.top).clamp(1, 4096);
    if width < 200 || height < 150 {
        return Ok(None);
    }

    let byte_len = (width as usize) * (height as usize) * 4;
    let mut pixels = vec![0u8; byte_len];

    unsafe {
        let hdc_screen = GetDC(None);
        if hdc_screen.is_invalid() {
            return Err("Could not acquire screen DC".to_string());
        }
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        if hdc_mem.is_invalid() {
            ReleaseDC(None, hdc_screen);
            return Err("Could not create compatible DC".to_string());
        }

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // top-down DIB
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };

        let mut bits_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbm = CreateDIBSection(hdc_screen, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
            .map_err(|e| {
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(None, hdc_screen);
                e.to_string()
            })?;

        let old_obj = SelectObject(hdc_mem, hbm);
        let blit_res = BitBlt(
            hdc_mem,
            0,
            0,
            width,
            height,
            hdc_screen,
            rect.left,
            rect.top,
            SRCCOPY,
        );

        if blit_res.is_ok() && !bits_ptr.is_null() {
            std::ptr::copy_nonoverlapping(bits_ptr as *const u8, pixels.as_mut_ptr(), byte_len);
        }

        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbm);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);

        if let Err(e) = blit_res {
            return Err(e.to_string());
        }
    }

    // Invert dark-mode UI pixels to black-on-white so Windows OCR reads thin fonts accurately
    let mut luma_sum: u64 = 0;
    let mut sample_count: u64 = 0;
    for chunk in pixels.chunks_exact(4).step_by(32) {
        luma_sum += (chunk[0] as u64 + chunk[1] as u64 + chunk[2] as u64) / 3;
        sample_count += 1;
    }
    if sample_count > 0 && (luma_sum / sample_count) < 115 {
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 255 - chunk[0];
            chunk[1] = 255 - chunk[1];
            chunk[2] = 255 - chunk[2];
            chunk[3] = 255;
        }
    } else {
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[3] = 255;
        }
    }

    let writer = DataWriter::new().map_err(|e| e.to_string())?;
    writer.WriteBytes(&pixels).map_err(|e| e.to_string())?;
    let buffer = writer.DetachBuffer().map_err(|e| e.to_string())?;
    let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width,
        height,
        BitmapAlphaMode::Ignore,
    )
    .map_err(|e| e.to_string())?;

    let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|e| e.to_string())?;
    let ocr_res = engine
        .RecognizeAsync(&bitmap)
        .map_err(|e| e.to_string())?
        .get()
        .map_err(|e| e.to_string())?;

    let mut text = String::new();
    if let Ok(lines) = ocr_res.Lines() {
        let count = lines.Size().unwrap_or(0);
        for i in 0..count {
            if let Ok(line) = lines.GetAt(i) {
                if let Ok(hstr) = line.Text() {
                    text.push_str(&hstr.to_string_lossy());
                    text.push('\n');
                }
            }
        }
    }

    Ok(parse_antigravity_settings_text(&text).ok())
}

pub fn scan_open_antigravity(allow_ocr: bool, manual: bool) -> AutoSyncMessage {
    let windows = find_antigravity_windows();
    if windows.is_empty() {
        return AutoSyncMessage::NotVisible { manual };
    }

    for &hwnd in &windows {
        if let Some(parsed) = try_read_via_uia(hwnd) {
            return AutoSyncMessage::Found(AutoSyncResult {
                parsed,
                source: SyncSource::UiAutomation,
                manual,
            });
        }
    }

    if allow_ocr {
        for &hwnd in windows.iter().take(2) {
            match try_read_via_ocr(hwnd) {
                Ok(Some(parsed)) => {
                    return AutoSyncMessage::Found(AutoSyncResult {
                        parsed,
                        source: SyncSource::ScreenOcr,
                        manual,
                    });
                }
                Ok(None) => {}
                Err(err) => {
                    return AutoSyncMessage::Error {
                        message: err,
                        manual,
                    };
                }
            }
        }
    }

    AutoSyncMessage::NotVisible { manual }
}
