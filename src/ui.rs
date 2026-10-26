use crate::{
    backend::{self, Event, Job, Report},
    fan_graph::{self, Graph},
    gpu,
    model::{self, FanCurve, Result},
};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    mem, ptr,
    sync::mpsc::{Receiver, Sender},
};
use windows::Win32::{
    System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    },
    UI::Accessibility::{CAccPropServices, IAccPropServices, PROPID_ACC_NAME},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::{
        CreateFontW, DeleteObject, GetStockObject, CLIP_DEFAULT_PRECIS, COLOR_WINDOW,
        DEFAULT_CHARSET, DEFAULT_GUI_FONT, DEFAULT_PITCH, DEFAULT_QUALITY, FW_NORMAL,
        OUT_DEFAULT_PRECIS,
    },
    System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
    UI::{
        Controls::{SetScrollInfo, EM_SETLIMITTEXT},
        HiDpi::{
            GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        },
        Input::KeyboardAndMouse::{EnableWindow, SetFocus},
        WindowsAndMessaging::*,
    },
};

const TABS: [&str; 4] = ["Overview", "CPU && battery", "Fan curves", "Capabilities"];
const POINT_FIELDS: [&str; 2] = ["Temperature degrees Celsius", "Fan speed percent"];
const APPLY_CPU: usize = 100;
const PL1: usize = 101;
const PL2: usize = 102;
const BATTERY: usize = 103;
const APPLY_BATTERY: usize = 104;
const RESTORE: usize = 110;
const READ_CURVE: usize = 120;
const APPLY_CURVE: usize = 121;
const SAVE_CURVE: usize = 122;
const LOAD_CURVE: usize = 123;
const BOOST: usize = 124;
const CPU_GRAPH: usize = 125;
const GPU_GRAPH: usize = 126;
const UPDATE_POINT: usize = 127;
const TELEMETRY: usize = 200;
const DETAILS: usize = 201;
const STATUS: usize = 202;
const CAPS: usize = 203;
const LIMITS: usize = 204;
const POINT_LABEL: usize = 205;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

pub fn error(value: &str) {
    unsafe {
        MessageBoxW(
            ptr::null_mut(),
            wide(value).as_ptr(),
            wide("NUCtool").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

struct Widget {
    handle: HWND,
    page: Option<usize>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

struct App {
    window: HWND,
    font: *mut c_void,
    scale: f64,
    widgets: Vec<Widget>,
    tx: Sender<Job>,
    rx: Receiver<Event>,
    initialized: bool,
    closing: bool,
    page: usize,
    scroll: i32,
    accessibility: IAccPropServices,
    graph: Option<Box<RefCell<Graph>>>,
    notice_visible: Cell<bool>,
}

impl App {
    fn control(
        &mut self,
        page: Option<usize>,
        id: usize,
        class: &str,
        text: &str,
        rect: (i32, i32, i32, i32),
        extra: u32,
    ) -> Result<HWND> {
        let (x, y, width, height) = rect;
        let handle = unsafe {
            CreateWindowExW(
                if class == "EDIT" { WS_EX_CLIENTEDGE } else { 0 },
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | extra,
                self.px(x),
                self.px(y),
                self.px(width),
                self.px(height),
                self.window,
                id as *mut c_void,
                GetModuleHandleW(ptr::null()),
                ptr::null(),
            )
        };
        if handle.is_null() {
            return Err(format!(
                "Cannot create a native control: {}",
                std::io::Error::last_os_error()
            ));
        }
        if (1000..1002).contains(&id) {
            let label = wide(POINT_FIELDS[id - 1000]);
            unsafe {
                self.accessibility.SetHwndPropStr(
                    windows::Win32::Foundation::HWND(handle),
                    OBJID_CLIENT as u32,
                    0,
                    PROPID_ACC_NAME,
                    windows::core::PCWSTR(label.as_ptr()),
                )
            }
            .map_err(|e| e.to_string())?;
        }
        unsafe {
            SendMessageW(handle, WM_SETFONT, self.font as usize, 1);
        }
        self.widgets.push(Widget {
            handle,
            page,
            x,
            y,
            width,
            height,
        });
        Ok(handle)
    }

    fn px(&self, value: i32) -> i32 {
        (f64::from(value) * self.scale).round() as i32
    }
    fn label(
        &mut self,
        page: Option<usize>,
        id: usize,
        text: &str,
        rect: (i32, i32, i32, i32),
    ) -> Result<()> {
        self.control(page, id, "STATIC", text, rect, 0)?;
        Ok(())
    }
    fn button(
        &mut self,
        page: Option<usize>,
        id: usize,
        text: &str,
        rect: (i32, i32, i32, i32),
    ) -> Result<()> {
        self.control(page, id, "BUTTON", text, rect, WS_TABSTOP)?;
        Ok(())
    }
    fn edit(&mut self, page: usize, id: usize, rect: (i32, i32, i32, i32)) -> Result<()> {
        let handle = self.control(
            Some(page),
            id,
            "EDIT",
            "",
            rect,
            WS_TABSTOP | ES_AUTOHSCROLL as u32,
        )?;
        unsafe {
            SendMessageW(handle, EM_SETLIMITTEXT, 8, 0);
        }
        Ok(())
    }
    fn text(&self, id: usize, value: &str) {
        unsafe {
            SetDlgItemTextW(self.window, id as i32, wide(value).as_ptr());
        }
        if id == STATUS {
            self.notice_visible.set(!value.is_empty());
            unsafe {
                ShowWindow(
                    GetDlgItem(self.window, STATUS as i32),
                    if value.is_empty() { SW_HIDE } else { SW_SHOW },
                );
            }
        }
    }
    fn value(&self, id: usize) -> String {
        let mut value = [0u16; 128];
        let length = unsafe {
            GetDlgItemTextW(
                self.window,
                id as i32,
                value.as_mut_ptr(),
                value.len() as i32,
            )
        };
        String::from_utf16_lossy(&value[..length as usize])
    }
    fn byte(&self, id: usize) -> Result<u8> {
        self.value(id)
            .trim()
            .parse()
            .map_err(|_| "Enter a whole number from 0 to 255.".into())
    }

    fn build(&mut self) -> Result<()> {
        for (page, label) in TABS.iter().enumerate() {
            self.button(
                None,
                10 + page,
                label,
                (20 + page as i32 * 180, 16, 170, 32),
            )?;
        }
        self.label(None, TELEMETRY, "Reading hardware…", (20, 62, 920, 54))?;
        self.label(None, STATUS, "", (20, 636, 725, 48))?;
        self.button(
            None,
            RESTORE,
            "Restore previous settings",
            (750, 640, 190, 32),
        )?;

        self.label(Some(0), DETAILS, "", (20, 145, 910, 300))?;

        self.label(Some(1), 0, "CPU power", (20, 136, 900, 26))?;
        for (index, label) in ["Power saver", "Balanced", "Performance"]
            .iter()
            .enumerate()
        {
            self.button(
                Some(1),
                130 + index,
                label,
                (20 + index as i32 * 180, 178, 170, 34),
            )?;
        }
        self.label(Some(1), LIMITS, "Reading CPU limits…", (20, 290, 900, 24))?;
        self.label(Some(1), 0, "Sustained PL1 (W)", (20, 332, 155, 24))?;
        self.edit(1, PL1, (180, 328, 80, 30))?;
        self.label(Some(1), 0, "Burst PL2 (W)", (285, 332, 145, 24))?;
        self.edit(1, PL2, (430, 328, 80, 30))?;
        self.button(Some(1), APPLY_CPU, "Apply CPU limits", (540, 328, 170, 32))?;
        self.label(Some(1), 0, "Battery charge limit (%)", (20, 410, 240, 26))?;
        self.edit(1, BATTERY, (265, 406, 80, 30))?;
        self.button(
            Some(1),
            APPLY_BATTERY,
            "Apply charge limit",
            (370, 406, 170, 32),
        )?;

        self.label(
            Some(2),
            0,
            "Drag points to edit temperature and speed.",
            (20, 130, 920, 24),
        )?;
        self.button(Some(2), CPU_GRAPH, "CPU fan", (20, 160, 140, 30))?;
        self.button(Some(2), GPU_GRAPH, "GPU fan", (175, 160, 140, 30))?;
        self.label(
            Some(2),
            POINT_LABEL,
            "Read a curve to begin.",
            (335, 164, 345, 28),
        )?;
        self.graph = Some(Graph::create(self.window, self.font)?);
        let graph_window = self.graph.as_ref().unwrap().borrow().window();
        self.widgets.push(Widget {
            handle: graph_window,
            page: Some(2),
            x: 20,
            y: 200,
            width: 920,
            height: 320,
        });
        unsafe { self.accessibility.SetHwndPropStr(windows::Win32::Foundation::HWND(graph_window),OBJID_CLIENT as u32,0,PROPID_ACC_NAME,windows::core::PCWSTR(wide("Fan curve. Left and right select a point; up and down change speed; Control with left or right changes temperature.").as_ptr())) }.map_err(|e|e.to_string())?;
        for (index, label) in ["Temperature °C", "Speed %"].into_iter().enumerate() {
            self.label(Some(2), 0, label, (20 + index as i32 * 270, 545, 135, 25))?;
            self.edit(2, 1000 + index, (160 + index as i32 * 270, 540, 90, 30))?;
        }
        self.button(Some(2), UPDATE_POINT, "Update point", (635, 540, 170, 32))?;
        for (i, (id, text)) in [
            (READ_CURVE, "Read hardware"),
            (APPLY_CURVE, "Apply curve"),
            (SAVE_CURVE, "Save curve"),
            (LOAD_CURVE, "Load saved"),
            (BOOST, "Full fan boost"),
        ]
        .into_iter()
        .enumerate()
        {
            self.button(Some(2), id, text, (20 + i as i32 * 185, 588, 175, 32))?;
        }

        self.label(Some(3), 0, "Capabilities", (20, 136, 900, 28))?;
        self.label(Some(3), CAPS, "Loading capabilities…", (20, 180, 915, 420))?;
        self.show_page(0);
        unsafe {
            SetTimer(self.window, 1, 100, None);
        }
        Ok(())
    }

    fn show_page(&mut self, page: usize) {
        self.page = page;
        for widget in &self.widgets {
            unsafe {
                ShowWindow(
                    widget.handle,
                    if widget.page.is_none_or(|p| p == page)
                        && (widget.handle != GetDlgItem(self.window, STATUS as i32)
                            || self.notice_visible.get())
                    {
                        SW_SHOW
                    } else {
                        SW_HIDE
                    },
                );
            }
        }
        unsafe {
            SetFocus(GetDlgItem(self.window, (10 + page) as i32));
        }
    }

    fn set_curve(&self, curve: FanCurve) {
        if curve.cpu.len() != 16 || curve.gpu.len() != 16 {
            self.text(STATUS, "The firmware did not return 16 entries per fan.");
            return;
        }
        if let Some(graph) = &self.graph {
            graph.borrow_mut().set_curve(curve);
        }
        self.sync_point();
    }

    fn curve(&self) -> Result<FanCurve> {
        self.commit_point()?;
        let curve = self
            .graph
            .as_ref()
            .ok_or("No fan editor.")?
            .borrow()
            .curve
            .clone()
            .ok_or("Read a hardware curve first.")?;
        curve.validate()?;
        Ok(curve)
    }

    fn commit_point(&self) -> Result<()> {
        self.graph
            .as_ref()
            .ok_or("No fan editor.")?
            .borrow_mut()
            .update_selected(self.byte(1000)?, model::parse_duty(&self.value(1001))?)
    }

    fn sync_point(&self) {
        if let Some(graph) = &self.graph {
            let graph = graph.borrow();
            if let Some(p) = graph.selected() {
                self.text(1000, &p.up.to_string());
                self.text(1001, &format!("{}", p.duty as f64 / 2.0));
                self.text(
                    POINT_LABEL,
                    &format!(
                        "{} · point {}",
                        if graph.fan == 0 { "CPU" } else { "GPU" },
                        graph.point + 1
                    ),
                );
            }
        }
    }

    fn report(&mut self, report: Report) {
        for id in [APPLY_CPU, APPLY_BATTERY, APPLY_CURVE, BOOST, 130, 131, 132] {
            let allowed = report.can_write
                && match id {
                    APPLY_CPU | 130..=132 => report.cpu_ceiling.is_some(),
                    APPLY_BATTERY => report.charge_limit.is_some(),
                    _ => true,
                };
            unsafe {
                EnableWindow(
                    GetDlgItem(self.window, id as i32),
                    i32::from(allowed && !self.closing),
                );
            }
        }
        let gpu = report.gpu.as_ref();
        let number = |value: Option<u32>, suffix: &str| {
            value
                .map(|v| format!("{v}{suffix}"))
                .unwrap_or_else(|| "unavailable".into())
        };
        self.text(TELEMETRY, &format!("CPU {}    GPU {}    Main fan {}    Secondary fan {}\r\nGPU draw {}    Graphics {}    Memory {}",
            number(report.cpu_c.map(u32::from), "°C"), number(gpu.and_then(|g| g.temperature_c).or(report.gpu_ec_c.map(u32::from)), "°C"), number(report.rpm[0].map(u32::from), " RPM"), number(report.rpm[1].map(u32::from), " RPM"),
            gpu.and_then(|g| g.power_mw).map(|v| format!("{:.1} W", v as f64 / 1000.0)).unwrap_or_else(|| "unavailable".into()), number(gpu.and_then(|g| g.graphics_mhz), " MHz"), number(gpu.and_then(|g| g.memory_mhz), " MHz")));
        self.text(
            LIMITS,
            &format!(
                "Current PL1/PL2: {} / {}. Firmware performance preset: {}.",
                number(report.cpu_limits[0].map(u32::from), " W"),
                number(report.cpu_limits[1].map(u32::from), " W"),
                report
                    .cpu_ceiling
                    .map(|(a, b)| format!("{a}/{b} W"))
                    .unwrap_or_else(|| "unavailable".into())
            ),
        );
        let inventory_gpus = report.machine.gpus.join(", ");
        let gpu_name = gpu
            .map(|g| g.name.as_str())
            .unwrap_or(if inventory_gpus.is_empty() {
                "unavailable"
            } else {
                &inventory_gpus
            });
        let cache = report
            .machine
            .cpu
            .l3_cache_size
            .map(|v| format!("{:.1} MB L3", v as f64 / 1024.0))
            .unwrap_or_else(|| "cache unavailable".into());
        self.text(
            DETAILS,
            &format!(
                "{}\r\n\r\n{}\r\n{} cores · {} threads · {}\r\n\r\n{}",
                report.machine.model,
                report.machine.cpu.name.trim(),
                number(report.machine.cpu.number_of_cores, ""),
                number(report.machine.cpu.number_of_logical_processors, ""),
                cache,
                gpu_name
            ),
        );
        self.text(CAPS,&format!("Hardware controls: {}\r\n{}\r\n\r\nGPU default power limit: {}\r\nGPU driver constraint range: {}\r\nGPU current power limit: {}\r\nGPU clock status: {}",
            if report.can_write {"available"}else{"read only"},report.ec_error.as_deref().unwrap_or(""),
            number(gpu.and_then(|g|g.default_limit_mw).map(|v|v/1000)," W"),
            gpu.and_then(|g|g.power_range_mw).map(|(min,max)|format!("{}–{} W",min/1000,max/1000)).unwrap_or_else(||"unavailable".into()),
            number(gpu.and_then(|g|g.power_limit_mw).map(|v|v/1000)," W"),gpu.and_then(|g|g.clock_event_reasons).map(gpu::clock_reasons).unwrap_or_else(||"unavailable".into())));
        self.text(
            BOOST,
            match report.fan_boost {
                Some(true) => "Turn boost off",
                Some(false) => "Full fan boost",
                None => "Fan boost unavailable",
            },
        );
        unsafe {
            EnableWindow(
                GetDlgItem(self.window, BOOST as i32),
                i32::from(report.can_write && report.fan_boost.is_some() && !self.closing),
            );
            EnableWindow(
                GetDlgItem(self.window, RESTORE as i32),
                i32::from(report.has_overrides && !self.closing),
            );
        }
        self.text(STATUS, &report.status);
        if !self.initialized {
            for (id, value) in [
                (PL1, report.cpu_limits[0]),
                (PL2, report.cpu_limits[1]),
                (BATTERY, report.charge_limit),
            ] {
                if let Some(value) = value {
                    self.text(id, &value.to_string());
                }
            }
            if let Some(curve) = report.curve {
                self.set_curve(curve);
            }
            self.initialized = true;
        }
    }

    fn command(&mut self, id: usize) -> Result<()> {
        if self.closing {
            return Ok(());
        }
        if (10..14).contains(&id) {
            self.show_page(id - 10);
            return Ok(());
        }
        if [CPU_GRAPH, GPU_GRAPH, UPDATE_POINT].contains(&id) {
            if id == UPDATE_POINT {
                self.commit_point()?;
            } else if let Some(graph) = &self.graph {
                graph.borrow_mut().select(usize::from(id == GPU_GRAPH), 0);
            }
            self.sync_point();
            return Ok(());
        }
        let job = match id {
            APPLY_CPU => Job::Cpu(self.byte(PL1)?, self.byte(PL2)?),
            APPLY_BATTERY => Job::Battery(self.byte(BATTERY)?),
            130..=132 => Job::Preset(id - 130),
            READ_CURVE => Job::ReadCurve,
            APPLY_CURVE => Job::Curve(self.curve()?),
            SAVE_CURVE => Job::SaveCurve(self.curve()?),
            LOAD_CURVE => Job::LoadCurve,
            BOOST => Job::Boost,
            RESTORE => {
                let answer = unsafe {
                    MessageBoxW(self.window, wide("Restore the settings captured before NUCtool changed them? This also replaces settings another program may have changed since then.").as_ptr(), wide("Restore previous settings").as_ptr(), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2)
                };
                if answer != IDYES {
                    return Ok(());
                }
                Job::Restore
            }
            _ => return Ok(()),
        };
        self.tx
            .send(job)
            .map_err(|_| "The hardware worker stopped. Recovery data was kept.".to_string())?;

        Ok(())
    }

    fn poll(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Report(report) => self.report(*report),
                Event::Curve(curve) => self.set_curve(curve),
                Event::Message(Ok(())) => (),
                Event::Message(Err(e)) => error(&e),
                Event::Fatal(e) => {
                    for id in [
                        APPLY_CPU,
                        APPLY_BATTERY,
                        APPLY_CURVE,
                        BOOST,
                        130,
                        131,
                        132,
                        RESTORE,
                    ] {
                        unsafe {
                            EnableWindow(GetDlgItem(self.window, id as i32), 0);
                        }
                    }
                    self.text(STATUS, &e);
                    error(&e);
                }
                Event::Quit(result) => {
                    if let Err(e) = result {
                        error(&format!("{e}\nRecovery data was kept for your next start."));
                    }
                    unsafe {
                        DestroyWindow(self.window);
                    }
                }
            }
        }
    }

    fn resize(&mut self, dpi: u32) {
        let scale = dpi as f64 / 96.0;
        let mut rect = unsafe { mem::zeroed() };
        unsafe {
            GetClientRect(self.window, &mut rect);
        }
        let max = ((704.0 * scale) as i32 - rect.bottom).max(0);
        self.scroll = self.scroll.clamp(0, max);
        let info = SCROLLINFO {
            cbSize: mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: (704.0 * scale) as i32 - 1,
            nPage: rect.bottom.max(0) as u32,
            nPos: self.scroll,
            nTrackPos: 0,
        };
        unsafe {
            SetScrollInfo(self.window, SB_VERT, &info, 1);
        }
        for w in &self.widgets {
            unsafe {
                MoveWindow(
                    w.handle,
                    (w.x as f64 * scale) as i32,
                    (w.y as f64 * scale) as i32 - self.scroll,
                    (w.width as f64 * scale) as i32,
                    (w.height as f64 * scale) as i32,
                    1,
                );
            }
        }
    }

    fn change_dpi(&mut self, dpi: u32) {
        self.scale = dpi as f64 / 96.0;
        let old = self.font;
        self.font = create_font(self.scale);
        for w in &self.widgets {
            unsafe {
                SendMessageW(w.handle, WM_SETFONT, self.font as usize, 1);
            }
        }
        if old != self.font {
            unsafe {
                DeleteObject(old);
            }
        }
        self.resize(dpi);
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // No Rust panic may unwind across the Windows callback boundary.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if message == WM_DESTROY {
            KillTimer(window, 1);
            PostQuitMessage(0);
            return 0;
        }
        if message == WM_NCDESTROY {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        }
        let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut RefCell<App>;
        let handled = matches!(
            message,
            WM_TIMER
                | WM_CLOSE
                | WM_DPICHANGED
                | WM_SIZE
                | WM_VSCROLL
                | WM_MOUSEWHEEL
                | WM_DESTROY
                | fan_graph::CHANGED
        ) || (message == WM_COMMAND && wparam >> 16 == BN_CLICKED as usize);
        if !pointer.is_null() && handled {
            let Ok(mut app) = (*pointer).try_borrow_mut() else {
                return 0;
            };
            match message {
                fan_graph::CHANGED => {
                    app.sync_point();
                    return 0;
                }
                WM_COMMAND if wparam >> 16 == BN_CLICKED as usize => {
                    if let Err(e) = app.command(wparam & 0xFFFF) {
                        error(&e);
                    }
                    return 0;
                }
                WM_TIMER => {
                    app.poll();
                    return 0;
                }
                WM_SIZE if !app.window.is_null() => {
                    let dpi = (app.scale * 96.0) as u32;
                    app.resize(dpi);
                    return 0;
                }
                WM_VSCROLL | WM_MOUSEWHEEL => {
                    let mut info: SCROLLINFO = mem::zeroed();
                    info.cbSize = mem::size_of::<SCROLLINFO>() as u32;
                    info.fMask = SIF_TRACKPOS;
                    GetScrollInfo(window, SB_VERT, &mut info);
                    let delta = if message == WM_MOUSEWHEEL {
                        -((wparam >> 16) as i16 as i32) / 120 * app.px(60)
                    } else {
                        match wparam as u16 as i32 {
                            SB_LINEUP => -app.px(24),
                            SB_LINEDOWN => app.px(24),
                            SB_PAGEUP => -app.px(200),
                            SB_PAGEDOWN => app.px(200),
                            SB_THUMBTRACK => info.nTrackPos - app.scroll,
                            _ => 0,
                        }
                    };
                    app.scroll += delta;
                    let dpi = (app.scale * 96.0) as u32;
                    app.resize(dpi);
                    return 0;
                }
                WM_CLOSE => {
                    if !app.closing {
                        app.closing = true;
                        for widget in &app.widgets {
                            EnableWindow(widget.handle, 0);
                        }
                        app.text(STATUS, "Restoring settings before closing…");
                        if app.tx.send(Job::Quit).is_err() {
                            DestroyWindow(window);
                        }
                    }
                    return 0;
                }
                WM_DPICHANGED => {
                    let rect = &*(lparam as *const windows_sys::Win32::Foundation::RECT);
                    SetWindowPos(
                        window,
                        ptr::null_mut(),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    app.change_dpi((wparam & 0xFFFF) as u32);
                    return 0;
                }
                WM_DESTROY => {
                    KillTimer(window, 1);
                    PostQuitMessage(0);
                    return 0;
                }
                _ => (),
            }
        }
        DefWindowProcW(window, message, wparam, lparam)
    }));
    result.unwrap_or_else(|_| {
        unsafe {
            PostQuitMessage(1);
        }
        0
    })
}

fn create_font(scale: f64) -> *mut c_void {
    let font = unsafe {
        CreateFontW(
            -(16.0 * scale) as i32,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            DEFAULT_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide("Segoe UI").as_ptr(),
        )
    };
    if font.is_null() {
        unsafe { GetStockObject(DEFAULT_GUI_FONT) }
    } else {
        font
    }
}

pub fn run() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }.map_err(|e| e.to_string())?;
    let _apartment = ComApartment;
    let accessibility = unsafe { CoCreateInstance(&CAccPropServices, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| e.to_string())?;
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mutex = unsafe { CreateMutexW(ptr::null(), 0, wide("Local\\NUCtoolRust").as_ptr()) };
    if mutex.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let _mutex = MutexHandle(mutex);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err("NUCtool is already running in this Windows session.".into());
    }
    let scale = unsafe { GetDpiForSystem() } as f64 / 96.0;
    let font = create_font(scale);
    let (tx, rx) = backend::start();
    let app = Box::new(RefCell::new(App {
        window: ptr::null_mut(),
        font,
        scale,
        widgets: Vec::new(),
        tx,
        rx,
        initialized: false,
        closing: false,
        page: 0,
        scroll: 0,
        accessibility,
        graph: None,
        notice_visible: Cell::new(false),
    }));
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let class_name = wide("NUCtoolRustWindow");
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class_name.as_ptr(),
        hCursor: unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) },
        hbrBackground: (COLOR_WINDOW + 1) as *mut c_void,
        ..unsafe { mem::zeroed() }
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let height = ((740.0 * scale) as i32).min(unsafe { GetSystemMetrics(SM_CYSCREEN) } - 60);
    let window = unsafe {
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class_name.as_ptr(),
            wide("NUCtool").as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VSCROLL,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            (980.0 * scale) as i32,
            height,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            (&*app as *const RefCell<App>).cast(),
        )
    };
    if window.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let _window = NativeWindow(window);
    app.borrow_mut().window = window;
    app.borrow_mut()
        .change_dpi(unsafe { GetDpiForWindow(window) });
    app.borrow_mut().build()?;
    app.borrow_mut().resize((scale * 96.0) as u32);
    unsafe {
        ShowWindow(window, SW_SHOW);
    }
    let mut message: MSG = unsafe { mem::zeroed() };
    loop {
        let status = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if status == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if status == 0 {
            break;
        }
        if unsafe { IsDialogMessageW(window, &message) } == 0 {
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    unsafe {
        DeleteObject(app.borrow().font);
    }
    Ok(())
}

struct MutexHandle(*mut c_void);
struct NativeWindow(HWND);
impl Drop for NativeWindow {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(self.0) != 0 {
                DestroyWindow(self.0);
            }
        }
    }
}
struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
impl Drop for MutexHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
