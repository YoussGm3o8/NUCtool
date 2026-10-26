use crate::model::{FanCurve, FanPoint, Result};
use std::{cell::RefCell, ffi::c_void, mem, ptr};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::GetDpiForWindow, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

pub const CHANGED: u32 = WM_APP + 1;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub struct Graph {
    pub curve: Option<FanCurve>,
    pub fan: usize,
    pub point: usize,
    window: HWND,
    font: *mut c_void,
    drag_start: Option<FanPoint>,
}

impl Graph {
    pub fn create(parent: HWND, font: *mut c_void) -> Result<Box<RefCell<Self>>> {
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let name = wide("NUCtoolFanGraph");
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            lpszClassName: name.as_ptr(),
            hCursor: unsafe { LoadCursorW(ptr::null_mut(), IDC_CROSS) },
            ..unsafe { mem::zeroed() }
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let graph = Box::new(RefCell::new(Self {
            curve: None,
            fan: 0,
            point: 0,
            window: ptr::null_mut(),
            font,
            drag_start: None,
        }));
        let window = unsafe {
            CreateWindowExW(WS_EX_CLIENTEDGE, name.as_ptr(), wide("Fan curve. Left and right select a point; up and down adjust speed; Control with left or right adjusts temperature.").as_ptr(), WS_CHILD | WS_VISIBLE | WS_TABSTOP, 0, 0, 1, 1, parent, ptr::null_mut(), instance, (&*graph as *const RefCell<Self>).cast())
        };
        if window.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        graph.borrow_mut().window = window;
        Ok(graph)
    }

    pub fn window(&self) -> HWND {
        self.window
    }
    pub fn selected(&self) -> Option<&FanPoint> {
        let curve = self.curve.as_ref()?;
        if self.fan == 0 {
            &curve.cpu
        } else {
            &curve.gpu
        }
        .get(self.point)
    }
    fn points(&self) -> &[FanPoint] {
        match &self.curve {
            Some(c) => {
                if self.fan == 0 {
                    &c.cpu
                } else {
                    &c.gpu
                }
            }
            None => &[],
        }
    }

    pub fn set_curve(&mut self, curve: FanCurve) {
        self.curve = Some(curve);
        self.point = 0;
        self.notify();
    }

    pub fn select(&mut self, fan: usize, point: usize) {
        self.fan = fan.min(1);
        self.point = point.min(15);
        self.notify();
    }

    pub fn update_selected(&mut self, temperature: u8, duty: u8) -> Result<()> {
        let curve = self.curve.as_mut().ok_or("Read a hardware curve first.")?;
        let old = curve.clone();
        let points = if self.fan == 0 {
            &mut curve.cpu
        } else {
            &mut curve.gpu
        };
        let target = points
            .get_mut(self.point)
            .ok_or("No selected curve point.")?;
        *target = target.adjusted(temperature, duty);
        if let Err(e) = curve.validate() {
            *curve = old;
            return Err(e);
        }
        self.notify();
        Ok(())
    }

    fn notify(&self) {
        unsafe {
            InvalidateRect(self.window, ptr::null(), 0);
            PostMessageW(GetParent(self.window), CHANGED, 0, 0);
        }
    }

    fn plot(&self) -> RECT {
        let mut rect = unsafe { mem::zeroed() };
        unsafe {
            GetClientRect(self.window, &mut rect);
        }
        let scale = unsafe { GetDpiForWindow(self.window) }.max(96) as f64 / 96.0;
        RECT {
            left: (55.0 * scale) as i32,
            top: (30.0 * scale) as i32,
            right: rect.right - (25.0 * scale) as i32,
            bottom: rect.bottom - (40.0 * scale) as i32,
        }
    }

    fn position(&self, temperature: u8, duty: u8) -> (i32, i32) {
        let r = self.plot();
        (
            r.left + (i32::from(temperature.min(110)) * (r.right - r.left) / 110),
            r.bottom - i32::from(duty.min(200)) * (r.bottom - r.top) / 200,
        )
    }

    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let radius = (12 * unsafe { GetDpiForWindow(self.window) }.max(96) / 96) as i32;
        self.points()
            .iter()
            .enumerate()
            .filter(|(_, p)| p.up != 255)
            .map(|(i, p)| {
                let (px, py) = self.position(p.up, p.duty);
                (i, (x - px).pow(2) + (y - py).pow(2))
            })
            .filter(|(_, distance)| *distance <= radius * radius)
            .min_by_key(|(_, distance)| *distance)
            .map(|(index, _)| index)
    }

    fn drag(&mut self, x: i32, y: i32) {
        let r = self.plot();
        if r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let temperature = ((x - r.left) * 110 / (r.right - r.left)).clamp(0, 110) as u8;
        let duty = ((r.bottom - y) * 200 / (r.bottom - r.top)).clamp(0, 200) as u8;
        if let Some(curve) = &mut self.curve {
            if move_point(curve, self.fan, self.point, temperature, duty).is_ok() {
                self.notify();
            }
        }
    }

    unsafe fn paint(&self, dc: HDC, rect: &RECT) {
        unsafe {
            FillRect(dc, rect, GetStockObject(WHITE_BRUSH) as _);
            SelectObject(dc, self.font);
            SetBkMode(dc, TRANSPARENT as i32);
            let r = self.plot();
            if r.right <= r.left || r.bottom <= r.top {
                return;
            }
            let grid = CreatePen(PS_SOLID, 1, 0x00E4E4E4);
            let old_pen = SelectObject(dc, grid);
            let text = |x, y, label: &str| {
                let label = wide(label);
                TextOutW(dc, x, y, label.as_ptr(), label.len() as i32 - 1);
            };
            SetTextColor(dc, 0x00505050);
            for temperature in (0..=110).step_by(10) {
                let (x, _) = self.position(temperature, 0);
                MoveToEx(dc, x, r.top, ptr::null_mut());
                LineTo(dc, x, r.bottom);
                text(x - 8, r.bottom + 8, &temperature.to_string());
            }
            for duty in (0..=200).step_by(40) {
                let (_, y) = self.position(0, duty);
                MoveToEx(dc, r.left, y, ptr::null_mut());
                LineTo(dc, r.right, y);
                text(6, y - 8, &format!("{}%", duty / 2));
            }
            text(r.left, 4, "Speed (%)");
            text(r.right - 135, r.bottom + 24, "Temperature (°C)");
            SelectObject(dc, old_pen);
            DeleteObject(grid);
            let color = if self.fan == 0 {
                0x00C07824
            } else {
                0x002F76CB
            };
            let pen = CreatePen(PS_SOLID, 2, color);
            let old_pen = SelectObject(dc, pen);
            let mut started = false;
            for point in self.points().iter().filter(|point| point.up != 255) {
                let (x, y) = self.position(point.up, point.duty);
                if started {
                    LineTo(dc, x, y);
                } else {
                    MoveToEx(dc, x, y, ptr::null_mut());
                    started = true;
                }
            }
            SelectObject(dc, old_pen);
            DeleteObject(pen);
            let brush = CreateSolidBrush(color);
            let old_brush = SelectObject(dc, brush);
            for (index, p) in self
                .points()
                .iter()
                .enumerate()
                .filter(|(_, p)| p.up != 255)
            {
                let (x, y) = self.position(p.up, p.duty);
                let radius = if index == self.point { 7 } else { 4 };
                Ellipse(dc, x - radius, y - radius, x + radius + 1, y + radius + 1);
            }
            SelectObject(dc, old_brush);
            DeleteObject(brush);
            if self.curve.is_none() {
                text(r.left + 20, r.top + 30, "Read the hardware curve to begin.");
            }
        }
    }
}

pub fn move_point(
    curve: &mut FanCurve,
    fan: usize,
    index: usize,
    temperature: u8,
    duty: u8,
) -> Result<()> {
    let points = if fan == 0 {
        &mut curve.cpu
    } else {
        &mut curve.gpu
    };
    let old = points.get(index).ok_or("No curve point.")?.clone();
    if old.up == 255 {
        return Err("Firmware sentinel entries are not draggable.".into());
    }
    let min_temperature = if index == 0 { 0 } else { points[index - 1].up };
    let max_temperature = points.get(index + 1).map_or(110, |p| p.up.min(110));
    let min_duty = if index == 0 {
        0
    } else {
        points[index - 1].duty
    };
    let max_duty = points.get(index + 1).map_or(200, |p| p.duty.min(200));
    if min_temperature > max_temperature || min_duty > max_duty {
        return Err("The existing curve needs valid ordering before dragging.".into());
    }
    let up = temperature.clamp(min_temperature, max_temperature);
    points[index] = old.adjusted(up, duty.clamp(min_duty, max_duty));
    if let Err(e) = curve.validate() {
        let points = if fan == 0 {
            &mut curve.cpu
        } else {
            &mut curve.gpu
        };
        points[index] = old;
        return Err(e);
    }
    Ok(())
}

unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if message == WM_NCDESTROY {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        }
        let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut RefCell<Graph>;
        if pointer.is_null() {
            return DefWindowProcW(window, message, wparam, lparam);
        }
        let Ok(mut graph) = (*pointer).try_borrow_mut() else {
            return DefWindowProcW(window, message, wparam, lparam);
        };
        match message {
            WM_GETDLGCODE => {
                return (DLGC_WANTARROWS
                    | if graph.drag_start.is_some() {
                        DLGC_WANTALLKEYS
                    } else {
                        0
                    }) as isize
            }
            WM_ERASEBKGND => return 1,
            WM_SIZE => {
                InvalidateRect(window, ptr::null(), 0);
                return 0;
            }
            WM_SETFONT => {
                graph.font = wparam as *mut c_void;
                return 0;
            }
            WM_PAINT => {
                let mut paint = mem::zeroed();
                let screen = BeginPaint(window, &mut paint);
                let mut rect = mem::zeroed();
                GetClientRect(window, &mut rect);
                let buffer = CreateCompatibleDC(screen);
                let bitmap = CreateCompatibleBitmap(screen, rect.right.max(1), rect.bottom.max(1));
                if !buffer.is_null() && !bitmap.is_null() {
                    let old = SelectObject(buffer, bitmap);
                    graph.paint(buffer, &rect);
                    BitBlt(screen, 0, 0, rect.right, rect.bottom, buffer, 0, 0, SRCCOPY);
                    SelectObject(buffer, old);
                } else {
                    graph.paint(screen, &rect);
                }
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                if !buffer.is_null() {
                    DeleteDC(buffer);
                }
                EndPaint(window, &paint);
                return 0;
            }
            WM_LBUTTONDOWN => {
                SetFocus(window);
                let x = lparam as i16 as i32;
                let y = (lparam >> 16) as i16 as i32;
                if let Some(index) = graph.hit(x, y) {
                    graph.point = index;
                    graph.drag_start = graph.selected().cloned();
                    SetCapture(window);
                    graph.notify();
                }
                return 0;
            }
            WM_MOUSEMOVE if graph.drag_start.is_some() => {
                graph.drag(lparam as i16 as i32, (lparam >> 16) as i16 as i32);
                return 0;
            }
            WM_LBUTTONUP | WM_CANCELMODE | WM_CAPTURECHANGED => {
                graph.drag_start = None;
                if message != WM_CAPTURECHANGED {
                    ReleaseCapture();
                }
                return 0;
            }
            WM_KEYDOWN => {
                let key = wparam as u16;
                if key == VK_ESCAPE {
                    if let Some(original) = graph.drag_start.take() {
                        let fan = graph.fan;
                        let index = graph.point;
                        if let Some(curve) = &mut graph.curve {
                            let points = if fan == 0 {
                                &mut curve.cpu
                            } else {
                                &mut curve.gpu
                            };
                            if let Some(point) = points.get_mut(index) {
                                *point = original;
                            }
                        }
                        ReleaseCapture();
                        graph.notify();
                    }
                    return 0;
                }
                if let Some(point) = graph.selected().cloned() {
                    if key == VK_UP || key == VK_DOWN {
                        let duty = if key == VK_UP {
                            point.duty.saturating_add(1)
                        } else {
                            point.duty.saturating_sub(1)
                        };
                        let fan = graph.fan;
                        let index = graph.point;
                        if let Some(curve) = &mut graph.curve {
                            let _ = move_point(curve, fan, index, point.up, duty);
                        }
                        graph.notify();
                    }
                    if key == VK_LEFT || key == VK_RIGHT {
                        if GetKeyState(VK_CONTROL as i32) < 0 {
                            let temperature = if key == VK_RIGHT {
                                point.up.saturating_add(1)
                            } else {
                                point.up.saturating_sub(1)
                            };
                            let fan = graph.fan;
                            let index = graph.point;
                            if let Some(curve) = &mut graph.curve {
                                let _ = move_point(curve, fan, index, temperature, point.duty);
                            }
                        } else {
                            let points = graph.points();
                            let next = if key == VK_RIGHT {
                                (graph.point + 1..points.len()).find(|i| points[*i].up != 255)
                            } else {
                                (0..graph.point).rev().find(|i| points[*i].up != 255)
                            };
                            if let Some(next) = next {
                                graph.point = next;
                            }
                        }
                        graph.notify();
                    }
                }
                return 0;
            }
            _ => (),
        }
        DefWindowProcW(window, message, wparam, lparam)
    }))
    .unwrap_or_else(|_| unsafe { DefWindowProcW(window, message, wparam, lparam) })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn curve() -> FanCurve {
        let points: Vec<_> = (0..16)
            .map(|i| FanPoint {
                up: 20 + i * 5,
                down: 17 + i * 5,
                duty: 50 + i * 10,
            })
            .collect();
        FanCurve {
            cpu: points.clone(),
            gpu: points,
        }
    }
    #[test]
    fn dragging_clamps_between_neighbors_and_keeps_hysteresis() {
        let mut c = curve();
        move_point(&mut c, 0, 5, 51, 108).unwrap();
        assert_eq!(
            c.cpu[5],
            FanPoint {
                up: 50,
                down: 47,
                duty: 108
            }
        );
    }
    #[test]
    fn dragging_cannot_remove_the_cooling_floor() {
        let mut c = curve();
        assert!(move_point(&mut c, 0, 15, 110, 200).is_ok());
        assert!(c.validate().is_ok());
        let mut weak = curve();
        for p in &mut weak.cpu {
            p.duty = 100;
        }
        weak.cpu[15].duty = 160;
        let old_weak = weak.clone();
        assert!(move_point(&mut weak, 0, 15, 110, 100).is_err());
        assert_eq!(weak, old_weak);
    }
    #[test]
    fn sentinel_entries_are_preserved() {
        let mut c = curve();
        c.cpu[15].up = 255;
        c.cpu[15].down = 255;
        let old = c.clone();
        assert!(move_point(&mut c, 0, 15, 90, 180).is_err());
        assert_eq!(c, old);
    }
}
