use crate::browser::{
    BrowserController, DownloadSnapshot, DownloadStatus, SharedController, TabSnapshot,
    lock_controller,
};
use crate::{storage, ui::pages};
use std::{
    ffi::c_void,
    iter, mem,
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};
use windows_sys::Win32::{
    Foundation::{
        COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT,
        POINT, RECT, WPARAM,
    },
    Graphics::{Dwm::DwmSetWindowAttribute, Gdi::*},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Controls::{
            Dialogs::{
                GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
                OPENFILENAMEW,
            },
            EM_SETMARGINS, EM_SETSEL, WM_MOUSELEAVE,
        },
        Input::KeyboardAndMouse::*,
        Shell::ShellExecuteW,
        WindowsAndMessaging::*,
    },
};

pub const TAB_HEIGHT: i32 = 44;
pub const TOOLBAR_HEIGHT: i32 = 108;
const INITIAL_WIDTH: i32 = 1200;
const INITIAL_HEIGHT: i32 = 800;
const MIN_WIDTH: i32 = 680;
const MIN_HEIGHT: i32 = 480;
const BACKGROUND: COLORREF = rgb(0xF1, 0xF2, 0xEC);
const SURFACE: COLORREF = rgb(0xFC, 0xFC, 0xFA);
const FOREGROUND: COLORREF = rgb(0x20, 0x22, 0x1D);
const SECONDARY: COLORREF = rgb(0x7C, 0x80, 0x73);
const DIVIDER: COLORREF = rgb(0xDE, 0xE0, 0xD7);
const HOVER: COLORREF = rgb(0xE7, 0xE9, 0xE0);
const ACCENT: COLORREF = rgb(0x72, 0x7C, 0x45);
const WARNING: COLORREF = rgb(0xB2, 0x6A, 0x28);
const CLOSE_HOVER: COLORREF = rgb(0xC5, 0x42, 0x3B);
const ID_ADDRESS: usize = 103;
const ID_DOWNLOAD_OVERLAY: usize = 110;
const ICON_ID: usize = 1;
const ANIMATION_TIMER: usize = 1;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: u32 = 2;
const MENU_PRIVATE: usize = 2001;
const MENU_HISTORY: usize = 2002;
const MENU_DOWNLOAD_PROMPT: usize = 2003;
const MENU_RESTORE: usize = 2004;
const MENU_CLEAR_HOUR: usize = 2005;
const MENU_CLEAR_DAY: usize = 2006;
const MENU_CLEAR_ALL: usize = 2007;
const MENU_CLEAR_SITE: usize = 2008;
const MENU_PIN: usize = 2101;
const MENU_CLOSE_TAB: usize = 2102;
static OPEN_WINDOWS: AtomicUsize = AtomicUsize::new(0);
static DOWNLOADS_HOOK: AtomicUsize = AtomicUsize::new(0);
static DOWNLOADS_OWNER: AtomicUsize = AtomicUsize::new(0);
const WM_CLOSE_DOWNLOADS: u32 = WM_APP + 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    Back,
    Forward,
    Reload,
    Home,
    NewTab,
    Downloads,
    Menu,
    Minimize,
    Maximize,
    CloseWindow,
    Tab(u64),
    TabClose(u64),
}

#[derive(Clone, Copy)]
struct Spring {
    value: f32,
    velocity: f32,
    target: f32,
}

impl Spring {
    fn new(value: f32) -> Self {
        Self {
            value,
            velocity: 0.0,
            target: value,
        }
    }

    fn step(&mut self, dt: f32) -> bool {
        let acceleration = 240.0 * (self.target - self.value) - 19.0 * self.velocity;
        self.velocity += acceleration * dt;
        self.value += self.velocity * dt;
        if (self.target - self.value).abs() < 0.025 && self.velocity.abs() < 0.04 {
            self.value = self.target;
            self.velocity = 0.0;
            false
        } else {
            true
        }
    }
}

struct TabVisual {
    tab: TabSnapshot,
    x: Spring,
    width: f32,
}

struct NativeUi {
    controller: SharedController,
    window: HWND,
    address: HWND,
    downloads_window: HWND,
    background_brush: HBRUSH,
    surface_brush: HBRUSH,
    font: HFONT,
    small_font: HFONT,
    icon_font: HFONT,
    title_font: HFONT,
    logo_icon: HICON,
    current_url: String,
    address_editing: bool,
    is_loading: bool,
    can_go_back: bool,
    can_go_forward: bool,
    tabs: Vec<TabVisual>,
    downloads: Vec<DownloadSnapshot>,
    show_downloads: bool,
    hover: Hit,
    pressed: Hit,
    drag_tab: Option<u64>,
    drag_origin: POINT,
    dragging: bool,
    panel_offset: Spring,
    animation_running: bool,
    rounded_by_dwm: bool,
    counted: bool,
}

impl NativeUi {
    fn new(controller: SharedController) -> Self {
        Self {
            controller,
            window: ptr::null_mut(),
            address: ptr::null_mut(),
            downloads_window: ptr::null_mut(),
            background_brush: ptr::null_mut(),
            surface_brush: ptr::null_mut(),
            font: ptr::null_mut(),
            small_font: ptr::null_mut(),
            icon_font: ptr::null_mut(),
            title_font: ptr::null_mut(),
            logo_icon: ptr::null_mut(),
            current_url: String::new(),
            address_editing: false,
            is_loading: false,
            can_go_back: false,
            can_go_forward: false,
            tabs: Vec::new(),
            downloads: Vec::new(),
            show_downloads: false,
            hover: Hit::None,
            pressed: Hit::None,
            drag_tab: None,
            drag_origin: POINT::default(),
            dragging: false,
            panel_offset: Spring::new(0.0),
            animation_running: false,
            rounded_by_dwm: true,
            counted: false,
        }
    }

    unsafe fn create_controls(&mut self, instance: HINSTANCE) -> Result<(), String> {
        unsafe {
            self.background_brush = CreateSolidBrush(BACKGROUND);
            self.surface_brush = CreateSolidBrush(SURFACE);
            self.font = create_font(16, "Segoe UI");
            self.small_font = create_font(12, "Segoe UI");
            self.icon_font = create_font(18, "Segoe UI Symbol");
            self.title_font = create_font(13, "Segoe UI");
            if self.background_brush.is_null()
                || self.surface_brush.is_null()
                || self.font.is_null()
                || self.small_font.is_null()
                || self.icon_font.is_null()
                || self.title_font.is_null()
            {
                return Err("Не удалось создать графические ресурсы панели.".to_owned());
            }
            self.address = create_child(
                "EDIT",
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                instance,
                self.window,
                ID_ADDRESS,
            )?;
            self.downloads_window = CreateWindowExW(
                0,
                wide("KockersBrowserOverlay").as_ptr(),
                ptr::null(),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                0,
                0,
                self.window,
                ID_DOWNLOAD_OVERLAY as HMENU,
                instance,
                ptr::null(),
            );
            if self.downloads_window.is_null() {
                return Err(format_windows_error("Не удалось создать панель загрузок"));
            }
            SendMessageW(self.address, WM_SETFONT, self.font as usize, 1);
            SendMessageW(
                self.address,
                EM_SETMARGINS,
                (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
                (34 | (18 << 16)) as isize,
            );
            subclass_control(self.address);
            self.logo_icon = LoadImageW(
                instance,
                ICON_ID as *const u16,
                IMAGE_ICON,
                28,
                28,
                LR_DEFAULTCOLOR,
            );
            if self.logo_icon.is_null() {
                return Err("Не удалось загрузить встроенный логотип K.".to_owned());
            }
            self.refresh_tabs();
            self.layout();
        }
        Ok(())
    }

    fn refresh_tabs(&mut self) {
        let snapshots = lock_controller(&self.controller).tabs_snapshot();
        let mut visuals = Vec::with_capacity(snapshots.len());
        for tab in snapshots {
            let x = self
                .tabs
                .iter()
                .find(|visual| visual.tab.id == tab.id)
                .map(|visual| visual.x)
                .unwrap_or_else(|| Spring::new(0.0));
            visuals.push(TabVisual {
                tab,
                x,
                width: 180.0,
            });
        }
        self.tabs = visuals;
        self.layout_tabs();
        self.update_animation_timer();
    }

    fn layout_tabs(&mut self) {
        let (width, _) = unsafe { client_size(self.window) };
        let origin = 52.0;
        let available = (width as f32 - 232.0).max(80.0);
        let pinned = self.tabs.iter().filter(|visual| visual.tab.pinned).count();
        let normal = self.tabs.len().saturating_sub(pinned);
        let pinned_width = 46.0;
        let normal_width = if normal == 0 {
            0.0
        } else {
            ((available - pinned as f32 * pinned_width) / normal as f32).clamp(68.0, 188.0)
        };
        let mut target = origin;
        for visual in &mut self.tabs {
            visual.width = if visual.tab.pinned {
                pinned_width
            } else {
                normal_width
            };
            if visual.x.value == 0.0 && visual.x.target == 0.0 {
                visual.x.value = target + 28.0;
                visual.x.velocity = 0.0;
            }
            visual.x.target = target;
            target += visual.width + 5.0;
        }
    }

    fn layout(&mut self) {
        if self.address.is_null() {
            return;
        }
        let (width, height) = unsafe { client_size(self.window) };
        self.layout_tabs();
        unsafe {
            MoveWindow(self.address, 184, 57, (width - 336).max(100), 40, 1);
            let region = CreateRoundRectRgn(0, 0, (width - 336).max(100) + 1, 41, 40, 40);
            if !region.is_null() {
                SetWindowRgn(self.address, region, 1);
            }
            self.layout_downloads();
            lock_controller(&self.controller).resize_content();
            if !self.rounded_by_dwm {
                if IsZoomed(self.window) != 0 {
                    SetWindowRgn(self.window, ptr::null_mut(), 1);
                } else {
                    let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, 16, 16);
                    if !region.is_null() {
                        SetWindowRgn(self.window, region, 1);
                    }
                }
            }
        }
    }

    fn layout_downloads(&mut self) {
        if self.downloads_window.is_null() {
            return;
        }
        let (width, height) = unsafe { client_size(self.window) };
        let panel_width = 400.min(width - 28);
        let content_height = if self.downloads.is_empty() {
            96
        } else {
            54 + self.downloads.len() as i32 * 74
        };
        let panel_height = content_height.min(height - TOOLBAR_HEIGHT - 32).max(90);
        unsafe {
            let mut current = RECT::default();
            GetClientRect(self.downloads_window, &mut current);
            MoveWindow(
                self.downloads_window,
                width - panel_width - 14,
                TOOLBAR_HEIGHT + 8 + self.panel_offset.value.round() as i32,
                panel_width,
                panel_height,
                1,
            );
            if current.right != panel_width || current.bottom != panel_height {
                let region = CreateRoundRectRgn(0, 0, panel_width + 1, panel_height + 1, 32, 32);
                if !region.is_null() {
                    SetWindowRgn(self.downloads_window, region, 1);
                }
            }
        }
    }

    fn update_address(&mut self, url: &str) {
        if url == pages::start_page_url() {
            self.current_url.clear();
            if !self.address_editing {
                set_window_text(self.address, "");
            }
        } else if !url.starts_with("data:") {
            self.current_url = url.to_owned();
            if !self.address_editing {
                set_window_text(self.address, url);
            }
            self.address_editing = false;
        }
        unsafe { InvalidateRect(self.address, ptr::null(), 1) };
    }

    fn sync_active(&mut self) {
        let (url, loading, back, forward) = {
            let state = lock_controller(&self.controller);
            let tab = state.tabs_snapshot().into_iter().find(|tab| tab.active);
            (
                tab.as_ref().map(|tab| tab.url.clone()).unwrap_or_default(),
                tab.as_ref().is_some_and(|tab| tab.loading),
                state.active_navigation().0,
                state.active_navigation().1,
            )
        };
        self.can_go_back = back;
        self.can_go_forward = forward;
        self.is_loading = loading;
        self.update_address(&url);
        unsafe { InvalidateRect(self.window, ptr::null(), 0) };
    }

    fn update_loading_state(&mut self, loading: bool, back: bool, forward: bool) {
        self.is_loading = loading;
        self.can_go_back = back;
        self.can_go_forward = forward;
        unsafe { InvalidateRect(self.window, ptr::null(), 0) };
    }

    fn update_downloads(&mut self) {
        self.downloads = lock_controller(&self.controller).downloads_snapshot();
        self.layout_downloads();
        unsafe {
            InvalidateRect(self.downloads_window, ptr::null(), 1);
            InvalidateRect(self.window, ptr::null(), 0);
        }
    }

    fn toggle_downloads(&mut self) {
        self.show_downloads = !self.show_downloads;
        if self.show_downloads {
            self.downloads = lock_controller(&self.controller).downloads_snapshot();
            self.panel_offset.value = 16.0;
            self.panel_offset.velocity = 0.0;
            self.panel_offset.target = 0.0;
            self.layout_downloads();
            unsafe {
                ShowWindow(self.downloads_window, SW_SHOW);
                SetWindowPos(
                    self.downloads_window,
                    HWND_TOP,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                );
            }
            self.update_animation_timer();
            unsafe { watch_outside_clicks(self.window) };
        } else {
            unsafe {
                ShowWindow(self.downloads_window, SW_HIDE);
                stop_watching_outside_clicks(self.window);
            }
        }
        unsafe { InvalidateRect(self.window, ptr::null(), 0) };
    }

    fn hide_downloads(&mut self) {
        if self.show_downloads {
            self.toggle_downloads();
        }
    }

    fn update_animation_timer(&mut self) {
        let needed = self.tabs.iter().any(|visual| {
            (visual.x.target - visual.x.value).abs() > 0.025 || visual.x.velocity.abs() > 0.04
        }) || (self.show_downloads
            && ((self.panel_offset.target - self.panel_offset.value).abs() > 0.025
                || self.panel_offset.velocity.abs() > 0.04));
        unsafe {
            if needed && !self.animation_running {
                SetTimer(self.window, ANIMATION_TIMER, 16, None);
                self.animation_running = true;
            } else if !needed && self.animation_running {
                KillTimer(self.window, ANIMATION_TIMER);
                self.animation_running = false;
            }
        }
    }

    fn animate(&mut self) {
        for visual in &mut self.tabs {
            visual.x.step(0.016);
        }
        if self.show_downloads {
            self.panel_offset.step(0.016);
            self.layout_downloads();
        }
        unsafe {
            InvalidateRect(self.window, ptr::null(), 0);
            InvalidateRect(self.downloads_window, ptr::null(), 0);
        }
        self.update_animation_timer();
    }

    fn submit_address(&mut self) {
        let input = window_text(self.address);
        self.address_editing = false;
        self.hide_downloads();
        BrowserController::navigate_input(&self.controller, &input);
    }

    fn cancel_address_editing(&mut self) {
        let current_url = self.current_url.clone();
        self.address_editing = false;
        set_window_text(self.address, &current_url);
        BrowserController::stop_loading(&self.controller);
    }

    fn hit_test(&self, x: i32, y: i32) -> Hit {
        let (width, _) = unsafe { client_size(self.window) };
        if y < TAB_HEIGHT {
            if x >= width - 46 {
                return Hit::CloseWindow;
            }
            if x >= width - 92 {
                return Hit::Maximize;
            }
            if x >= width - 138 {
                return Hit::Minimize;
            }
            if x >= width - 180 && x < width - 138 {
                return Hit::NewTab;
            }
            for visual in &self.tabs {
                let left = visual.x.value.round() as i32;
                let right = left + visual.width.round() as i32;
                if x >= left && x < right && y >= 5 {
                    if !visual.tab.pinned && x >= right - 32 {
                        return Hit::TabClose(visual.tab.id);
                    }
                    return Hit::Tab(visual.tab.id);
                }
            }
        } else if (54..101).contains(&y) {
            match x {
                14..=49 => return Hit::Back,
                56..=91 => return Hit::Forward,
                98..=133 => return Hit::Reload,
                140..=175 => return Hit::Home,
                _ => {}
            }
            if x >= width - 102 && x < width - 56 {
                return Hit::Downloads;
            }
            if x >= width - 50 {
                return Hit::Menu;
            }
        }
        Hit::None
    }

    fn activate_hit(&mut self, hit: Hit) {
        match hit {
            Hit::Back => BrowserController::go_back(&self.controller),
            Hit::Forward => BrowserController::go_forward(&self.controller),
            Hit::Reload => BrowserController::reload_or_stop(&self.controller),
            Hit::Home => BrowserController::go_home(&self.controller),
            Hit::NewTab => {
                BrowserController::new_tab(&self.controller, pages::start_page_url(), false)
            }
            Hit::Downloads => self.toggle_downloads(),
            Hit::Menu => unsafe { show_main_menu(self.window) },
            Hit::Minimize => unsafe {
                ShowWindow(self.window, SW_MINIMIZE);
            },
            Hit::Maximize => unsafe {
                ShowWindow(
                    self.window,
                    if IsZoomed(self.window) != 0 {
                        SW_RESTORE
                    } else {
                        SW_MAXIMIZE
                    },
                );
            },
            Hit::CloseWindow => unsafe {
                PostMessageW(self.window, WM_CLOSE, 0, 0);
            },
            Hit::Tab(id) => BrowserController::activate_tab(&self.controller, id),
            Hit::TabClose(id) => BrowserController::close_tab(&self.controller, id),
            Hit::None => {}
        }
    }

    unsafe fn destroy_resources(&mut self) {
        unsafe {
            if self.animation_running {
                KillTimer(self.window, ANIMATION_TIMER);
            }
            if !self.logo_icon.is_null() {
                DestroyIcon(self.logo_icon);
                self.logo_icon = ptr::null_mut();
            }
            for object in [
                self.background_brush as HGDIOBJ,
                self.surface_brush as HGDIOBJ,
                self.font as HGDIOBJ,
                self.small_font as HGDIOBJ,
                self.icon_font as HGDIOBJ,
                self.title_font as HGDIOBJ,
            ] {
                if !object.is_null() {
                    DeleteObject(object);
                }
            }
        }
    }
}

pub fn create_main_window(controller: SharedController) -> Result<HWND, String> {
    unsafe {
        let instance = GetModuleHandleW(wide("kockers_browser.dll").as_ptr());
        if instance.is_null() {
            return Err("Windows не вернула HMODULE sandbox DLL приложения.".to_owned());
        }
        register_classes(instance)?;
        let icon = LoadImageW(
            instance,
            ICON_ID as *const u16,
            IMAGE_ICON,
            32,
            32,
            LR_DEFAULTCOLOR,
        );
        let small_icon = LoadImageW(
            instance,
            ICON_ID as *const u16,
            IMAGE_ICON,
            16,
            16,
            LR_DEFAULTCOLOR,
        );
        let private = lock_controller(&controller).is_private();
        let title = if private {
            "Приватное окно — KOCKERS"
        } else {
            "KOCKERS"
        };
        let ui = Box::new(NativeUi::new(controller));
        let ui_ptr = Box::into_raw(ui);
        let window = CreateWindowExW(
            WS_EX_APPWINDOW,
            wide("KockersBrowserWindow").as_ptr(),
            wide(title).as_ptr(),
            WS_POPUP
                | WS_THICKFRAME
                | WS_MINIMIZEBOX
                | WS_MAXIMIZEBOX
                | WS_CLIPCHILDREN
                | WS_CLIPSIBLINGS,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            INITIAL_WIDTH,
            INITIAL_HEIGHT,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ui_ptr.cast(),
        );
        if window.is_null() {
            drop(Box::from_raw(ui_ptr));
            return Err(format_windows_error("Не удалось создать главное окно"));
        }
        let ui = &mut *ui_ptr;
        ui.window = window;
        if let Err(error) = ui.create_controls(instance) {
            DestroyWindow(window);
            return Err(error);
        }
        let preference = DWMWCP_ROUND;
        ui.rounded_by_dwm = DwmSetWindowAttribute(
            window,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const u32 as *const c_void,
            mem::size_of::<u32>() as u32,
        ) >= 0;
        if !small_icon.is_null() {
            SendMessageW(window, WM_SETICON, ICON_SMALL as usize, small_icon as isize);
        }
        if !icon.is_null() {
            SendMessageW(window, WM_SETICON, ICON_BIG as usize, icon as isize);
        }
        OPEN_WINDOWS.fetch_add(1, Ordering::SeqCst);
        ui.counted = true;
        ShowWindow(window, SW_SHOW);
        UpdateWindow(window);
        Ok(window)
    }
}

unsafe fn register_classes(instance: HINSTANCE) -> Result<(), String> {
    unsafe {
        let class_name = wide("KockersBrowserWindow");
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: LoadImageW(
                instance,
                ICON_ID as *const u16,
                IMAGE_ICON,
                32,
                32,
                LR_DEFAULTCOLOR,
            ),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(format_windows_error(
                "Не удалось зарегистрировать класс окна",
            ));
        }
        let overlay_name = wide("KockersBrowserOverlay");
        let overlay = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_proc),
            hInstance: instance,
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            lpszClassName: overlay_name.as_ptr(),
            ..Default::default()
        };
        if RegisterClassW(&overlay) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(format_windows_error("Не удалось зарегистрировать панель"));
        }
        Ok(())
    }
}

/// # Safety
/// `window` должен быть действительным HWND текущего UI-потока.
pub unsafe fn client_size(window: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    unsafe {
        if GetClientRect(window, &mut rect) == 0 {
            return (INITIAL_WIDTH, INITIAL_HEIGHT);
        }
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// # Safety
/// Все HWND должны быть действительными дочерними окнами текущего UI-потока.
pub unsafe fn resize_browsers(window: HWND, browsers: &[(u64, HWND)], active: Option<u64>) {
    let (width, height) = unsafe { client_size(window) };
    for (id, browser) in browsers {
        if !browser.is_null() {
            unsafe {
                MoveWindow(
                    *browser,
                    0,
                    TOOLBAR_HEIGHT,
                    width,
                    (height - TOOLBAR_HEIGHT).max(0),
                    i32::from(active == Some(*id)),
                );
            }
        }
    }
}

pub(crate) fn apply_browser_visibility(
    window: HWND,
    browsers: &[(u64, HWND)],
    active: Option<u64>,
) {
    unsafe { resize_browsers(window, browsers, active) };
    for (id, handle) in browsers {
        if handle.is_null() {
            continue;
        }
        unsafe {
            ShowWindow(
                *handle,
                if active == Some(*id) {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
        }
    }
}

pub(crate) fn refresh_tabs(window: HWND) {
    if let Some(ui) = ui_mut(window) {
        ui.refresh_tabs();
        unsafe { InvalidateRect(window, ptr::null(), 0) };
    }
}

pub fn sync_active_tab(window: HWND) {
    if let Some(ui) = ui_mut(window) {
        ui.sync_active();
    }
}

pub fn update_address(window: HWND, url: &str) {
    if let Some(ui) = ui_mut(window) {
        ui.update_address(url);
    }
}

pub fn update_title(window: HWND, title: &str) {
    set_window_text(window, title);
}

pub fn update_loading_state(window: HWND, loading: bool, back: bool, forward: bool) {
    if let Some(ui) = ui_mut(window) {
        ui.update_loading_state(loading, back, forward);
    }
}

pub fn update_downloads(window: HWND) {
    if let Some(ui) = ui_mut(window) {
        ui.update_downloads();
    }
}

/// # Safety
/// `window` должен быть HWND главного окна текущего UI-потока.
pub unsafe fn focus_address(window: HWND) {
    if let Some(ui) = ui_mut(window) {
        ui.address_editing = false;
        unsafe {
            SetForegroundWindow(window);
            SetFocus(ui.address);
            SendMessageW(ui.address, EM_SETSEL, 0, -1);
        }
    }
}

pub fn finish_address_editing(window: HWND) {
    if let Some(ui) = ui_mut(window) {
        ui.address_editing = false;
    }
}

pub(crate) fn finish_window_close(window: HWND) {
    if !window.is_null() {
        unsafe { DestroyWindow(window) };
    }
}

pub fn show_direct_launch_error() {
    show_fatal_error(
        "Для sandbox-сборки запускайте kockers_browser.exe из каталога target/bundle. Его создаёт команда kockers-bundle.",
    );
}

pub fn show_fatal_error(message: &str) {
    unsafe {
        MessageBoxW(
            ptr::null_mut(),
            wide(message).as_ptr(),
            wide("KOCKERS — ошибка").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn choose_download_path(window: HWND, name: &str, ask: bool) -> Option<PathBuf> {
    if !ask {
        let directory = storage::downloads_dir()?;
        std::fs::create_dir_all(&directory).ok()?;
        return Some(directory.join(name));
    }
    unsafe {
        let mut file = vec![0u16; 32_768];
        let initial = wide(name);
        let copy_len = initial.len().min(file.len() - 1);
        file[..copy_len].copy_from_slice(&initial[..copy_len]);
        let mut filter = "Все файлы".encode_utf16().collect::<Vec<_>>();
        filter.push(0);
        filter.extend("*.*".encode_utf16());
        filter.extend([0, 0]);
        let mut dialog = OPENFILENAMEW {
            lStructSize: mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: window,
            lpstrFilter: filter.as_ptr(),
            lpstrFile: file.as_mut_ptr(),
            nMaxFile: file.len() as u32,
            Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
            ..Default::default()
        };
        if GetSaveFileNameW(&mut dialog) == 0 {
            return None;
        }
        let length = file
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(file.len());
        Some(PathBuf::from(String::from_utf16_lossy(&file[..length])))
    }
}

pub(crate) fn open_downloaded_file(path: &Path) {
    let dangerous = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            [
                "exe", "msi", "bat", "cmd", "com", "scr", "ps1", "js", "vbs", "jar", "lnk",
            ]
            .iter()
            .any(|value| extension.eq_ignore_ascii_case(value))
        });
    if dangerous {
        let accepted = unsafe {
            MessageBoxW(
                ptr::null_mut(),
                wide("Этот файл может запустить программу или изменить систему. Открыть его только если вы доверяете источнику.").as_ptr(),
                wide("KOCKERS — предупреждение").as_ptr(),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
            ) == IDYES
        };
        if !accepted {
            return;
        }
    }
    unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            wide("open").as_ptr(),
            wide(&path.to_string_lossy()).as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

pub(crate) fn confirm_permission(window: HWND, origin: &str, subject: &str) -> bool {
    unsafe {
        MessageBoxW(
            window,
            wide(&format!("Сайт {origin} запрашивает доступ к {subject}. Разрешить только при доверии к сайту.")).as_ptr(),
            wide("KOCKERS — разрешение сайта").as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_NCCREATE => {
                let create = &*(lparam as *const CREATESTRUCTW);
                SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
                return 1;
            }
            WM_NCCALCSIZE => {
                if wparam != 0 {
                    return 0;
                }
            }
            WM_NCHITTEST => return non_client_hit_test(window, lparam),
            WM_ERASEBKGND => return 1,
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(window, &mut paint);
                if let Some(ui) = ui_mut(window) {
                    paint_toolbar(ui, dc);
                }
                EndPaint(window, &paint);
                return 0;
            }
            WM_SIZE | WM_DPICHANGED => {
                if let Some(ui) = ui_mut(window) {
                    ui.layout();
                }
                if message == WM_DPICHANGED {
                    let suggested = &*(lparam as *const RECT);
                    SetWindowPos(
                        window,
                        ptr::null_mut(),
                        suggested.left,
                        suggested.top,
                        suggested.right - suggested.left,
                        suggested.bottom - suggested.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                return 0;
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam as *mut MINMAXINFO);
                info.ptMinTrackSize.x = MIN_WIDTH;
                info.ptMinTrackSize.y = MIN_HEIGHT;
                return 0;
            }
            WM_CLOSE_DOWNLOADS => {
                if let Some(ui) = ui_mut(window) {
                    ui.hide_downloads();
                }
                return 0;
            }
            WM_ACTIVATE if (wparam & 0xFFFF) as u32 == WA_INACTIVE => {
                if let Some(ui) = ui_mut(window) {
                    ui.hide_downloads();
                }
            }
            WM_TIMER if wparam == ANIMATION_TIMER => {
                if let Some(ui) = ui_mut(window) {
                    ui.animate();
                }
                return 0;
            }
            WM_MOUSEMOVE => {
                let x = low_word(lparam) as i32;
                let y = high_word(lparam) as i32;
                if let Some(ui) = ui_mut(window) {
                    if let Some(id) = ui.drag_tab {
                        if !ui.dragging
                            && ((x - ui.drag_origin.x).abs() > 5
                                || (y - ui.drag_origin.y).abs() > 5)
                        {
                            ui.dragging = true;
                        }
                        if ui.dragging {
                            let target = ui
                                .tabs
                                .iter()
                                .position(|visual| {
                                    let center = visual.x.value + visual.width / 2.0;
                                    (x as f32) < center
                                })
                                .unwrap_or(ui.tabs.len().saturating_sub(1));
                            BrowserController::reorder_tab(&ui.controller, id, target);
                        }
                    }
                    let hit = ui.hit_test(x, y);
                    if hit != ui.hover {
                        ui.hover = hit;
                        InvalidateRect(window, ptr::null(), 0);
                    }
                    track_mouse_leave(window);
                }
                return 0;
            }
            WM_MOUSELEAVE => {
                if let Some(ui) = ui_mut(window) {
                    ui.hover = Hit::None;
                    InvalidateRect(window, ptr::null(), 0);
                }
                return 0;
            }
            WM_LBUTTONDOWN => {
                let x = low_word(lparam) as i32;
                let y = high_word(lparam) as i32;
                if let Some(ui) = ui_mut(window) {
                    let hit = ui.hit_test(x, y);
                    ui.pressed = hit;
                    if let Hit::Tab(id) = hit {
                        ui.drag_tab = Some(id);
                        ui.drag_origin = POINT { x, y };
                        ui.dragging = false;
                    }
                    SetCapture(window);
                    InvalidateRect(window, ptr::null(), 0);
                }
                return 0;
            }
            WM_LBUTTONUP => {
                let x = low_word(lparam) as i32;
                let y = high_word(lparam) as i32;
                if let Some(ui) = ui_mut(window) {
                    let hit = ui.hit_test(x, y);
                    let pressed = ui.pressed;
                    let dragging = ui.dragging;
                    ui.pressed = Hit::None;
                    ui.drag_tab = None;
                    ui.dragging = false;
                    ReleaseCapture();
                    if !dragging && pressed == hit {
                        ui.activate_hit(hit);
                    }
                    InvalidateRect(window, ptr::null(), 0);
                }
                return 0;
            }
            WM_MBUTTONUP => {
                let x = low_word(lparam) as i32;
                let y = high_word(lparam) as i32;
                if let Some(ui) = ui_mut(window)
                    && let Hit::Tab(id) | Hit::TabClose(id) = ui.hit_test(x, y)
                {
                    BrowserController::close_tab(&ui.controller, id);
                }
                return 0;
            }
            WM_RBUTTONUP => {
                let x = low_word(lparam) as i32;
                let y = high_word(lparam) as i32;
                if let Some(ui) = ui_mut(window)
                    && let Hit::Tab(id) = ui.hit_test(x, y)
                {
                    show_tab_menu(window, id);
                }
                return 0;
            }
            WM_COMMAND => {
                let id = wparam & 0xFFFF;
                let notification = (wparam >> 16) & 0xFFFF;
                if notification == 0 && id == MENU_PRIVATE {
                    BrowserController::open_private_window();
                    return 0;
                }
                if id == ID_ADDRESS {
                    if notification == EN_CHANGE as usize {
                        if let Some(ui) = ui_mut(window) {
                            ui.address_editing = true;
                            InvalidateRect(ui.address, ptr::null(), 0);
                        }
                    } else if notification == EN_KILLFOCUS as usize {
                        finish_address_editing(window);
                    }
                    return 0;
                }
            }
            WM_CTLCOLOREDIT => {
                if let Some(ui) = ui_mut(window) {
                    SetBkColor(wparam as HDC, SURFACE);
                    SetTextColor(wparam as HDC, FOREGROUND);
                    return ui.surface_brush as LRESULT;
                }
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if handle_native_shortcut(window, wparam as i32) {
                    return 0;
                }
            }
            WM_CLOSE => {
                let should_close = ui_mut(window)
                    .map(|ui| ui.controller.clone())
                    .is_none_or(|controller| BrowserController::close_requested(&controller));
                if should_close {
                    DestroyWindow(window);
                }
                return 0;
            }
            WM_NCDESTROY => {
                stop_watching_outside_clicks(window);
                let ui_pointer = ui_ptr(window);
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                if !ui_pointer.is_null() {
                    let mut ui = Box::from_raw(ui_pointer);
                    ui.destroy_resources();
                    if ui.counted && OPEN_WINDOWS.fetch_sub(1, Ordering::SeqCst) == 1 {
                        cef::quit_message_loop();
                    }
                }
                return DefWindowProcW(window, message, wparam, lparam);
            }
            _ => {}
        }
        DefWindowProcW(window, message, wparam, lparam)
    }
}

unsafe extern "system" fn overlay_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let parent = GetParent(window);
        match message {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(window, &mut paint);
                if let Some(ui) = ui_mut(parent) {
                    draw_download_panel(ui, dc);
                }
                EndPaint(window, &paint);
                0
            }
            WM_LBUTTONUP => {
                let x = low_word(lparam) as i32;
                let y = high_word(lparam) as i32;
                if let Some(ui) = ui_mut(parent) {
                    let (width, _) = client_size(window);
                    if y < 50 && x > width - 48 {
                        ui.toggle_downloads();
                    } else if y >= 54 {
                        let index = ((y - 54) / 74) as usize;
                        if let Some(download) = ui.downloads.get(index).cloned() {
                            if x >= width - 58 {
                                BrowserController::cancel_download(&ui.controller, download.id);
                            } else if x >= width - 124 {
                                if download.status == DownloadStatus::Complete {
                                    BrowserController::open_download(&ui.controller, download.id);
                                } else {
                                    BrowserController::toggle_download_pause(
                                        &ui.controller,
                                        download.id,
                                    );
                                }
                            }
                        }
                    }
                }
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

unsafe extern "system" fn control_proc(
    control: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let window = GetParent(control);
        if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
            if handle_native_shortcut(window, wparam as i32) {
                return 0;
            }
            if let Some(ui) = ui_mut(window)
                && control == ui.address
                && wparam as i32 == VK_ESCAPE as i32
            {
                ui.cancel_address_editing();
                return 0;
            }
        }
        if message == WM_KEYDOWN
            && wparam as i32 == 0x0D
            && let Some(ui) = ui_mut(window)
            && control == ui.address
        {
            ui.submit_address();
            return 0;
        }
        if matches!(message, WM_SETFOCUS | WM_KILLFOCUS | WM_MOUSEMOVE) {
            InvalidateRect(control, ptr::null(), 0);
        }
        let previous = GetWindowLongPtrW(control, GWLP_USERDATA);
        if previous == 0 {
            return DefWindowProcW(control, message, wparam, lparam);
        }
        let previous_proc: WNDPROC = mem::transmute::<isize, WNDPROC>(previous);
        let result = match previous_proc {
            Some(previous_proc) => {
                CallWindowProcW(Some(previous_proc), control, message, wparam, lparam)
            }
            None => DefWindowProcW(control, message, wparam, lparam),
        };
        if message == WM_PAINT {
            draw_address_overlay(control);
        }
        if message == WM_NCDESTROY {
            SetWindowLongPtrW(control, GWLP_WNDPROC, previous);
            SetWindowLongPtrW(control, GWLP_USERDATA, 0);
        }
        result
    }
}

unsafe fn draw_address_overlay(address: HWND) {
    unsafe {
        let parent = GetParent(address);
        let Some(ui) = ui_mut(parent) else { return };
        let text = window_text(address);
        if !text.is_empty() && !ui.current_url.starts_with("http://") {
            return;
        }
        let dc = GetDC(address);
        if dc.is_null() {
            return;
        }
        let mut rect = RECT::default();
        GetClientRect(address, &mut rect);
        SetBkMode(dc, TRANSPARENT as i32);
        SelectObject(dc, ui.font as HGDIOBJ);
        if ui.current_url.starts_with("http://") {
            SetTextColor(dc, WARNING);
            let mut warning = wide("!");
            let mut warning_rect = RECT {
                left: 14,
                top: 0,
                right: 28,
                bottom: rect.bottom,
            };
            DrawTextW(
                dc,
                warning.as_mut_ptr(),
                1,
                &mut warning_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        if text.is_empty() {
            SetTextColor(dc, SECONDARY);
            let mut placeholder = wide("Адрес или поиск");
            rect.left = 34;
            rect.right -= 18;
            DrawTextW(
                dc,
                placeholder.as_mut_ptr(),
                placeholder.len() as i32 - 1,
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
        ReleaseDC(address, dc);
    }
}

fn handle_native_shortcut(window: HWND, key: i32) -> bool {
    let control = key_down(VK_CONTROL as i32);
    let shift = key_down(VK_SHIFT as i32);
    let alt = key_down(VK_MENU as i32);
    let Some(ui) = ui_mut(window) else {
        return false;
    };
    let controller = ui.controller.clone();
    match key {
        0x4C if control => unsafe { focus_address(window) },
        0x52 if control => BrowserController::reload(&controller),
        0x54 if control && shift => BrowserController::restore_closed_tab(&controller),
        0x54 if control => BrowserController::new_tab(&controller, pages::start_page_url(), false),
        0x57 if control => BrowserController::close_active_tab(&controller),
        0x4E if control && shift => BrowserController::open_private_window(),
        value if value == VK_TAB as i32 && control => {
            BrowserController::next_tab(&controller, shift)
        }
        value if value == VK_F5 as i32 => BrowserController::reload(&controller),
        value if value == VK_LEFT as i32 && alt => BrowserController::go_back(&controller),
        value if value == VK_RIGHT as i32 && alt => BrowserController::go_forward(&controller),
        value if value == VK_F12 as i32 => BrowserController::show_dev_tools(&controller),
        value if value == VK_ESCAPE as i32 => BrowserController::stop_loading(&controller),
        value if control && ('1' as i32..='9' as i32).contains(&value) => {
            let index = (value - '1' as i32) as usize;
            let id = {
                let state = lock_controller(&controller);
                if value == '9' as i32 {
                    state.tabs_snapshot().last().map(|tab| tab.id)
                } else {
                    state.tabs_snapshot().get(index).map(|tab| tab.id)
                }
            };
            if let Some(id) = id {
                BrowserController::activate_tab(&controller, id);
            }
        }
        _ => return false,
    }
    true
}

unsafe fn subclass_control(control: HWND) {
    unsafe {
        let previous = SetWindowLongPtrW(control, GWLP_WNDPROC, control_proc as *const () as isize);
        SetWindowLongPtrW(control, GWLP_USERDATA, previous);
    }
}

unsafe fn non_client_hit_test(window: HWND, lparam: LPARAM) -> LRESULT {
    unsafe {
        let mut point = POINT {
            x: low_word(lparam) as i32,
            y: high_word(lparam) as i32,
        };
        ScreenToClient(window, &mut point);
        let (width, height) = client_size(window);
        let zoomed = IsZoomed(window) != 0;
        let border = 6;
        if !zoomed {
            let left = point.x < border;
            let right = point.x >= width - border;
            let top = point.y < border;
            let bottom = point.y >= height - border;
            match (left, right, top, bottom) {
                (true, _, true, _) => return HTTOPLEFT as LRESULT,
                (_, true, true, _) => return HTTOPRIGHT as LRESULT,
                (true, _, _, true) => return HTBOTTOMLEFT as LRESULT,
                (_, true, _, true) => return HTBOTTOMRIGHT as LRESULT,
                (true, ..) => return HTLEFT as LRESULT,
                (_, true, ..) => return HTRIGHT as LRESULT,
                (_, _, true, _) => return HTTOP as LRESULT,
                (_, _, _, true) => return HTBOTTOM as LRESULT,
                _ => {}
            }
        }
        if point.y < TAB_HEIGHT
            && let Some(ui) = ui_mut(window)
            && ui.hit_test(point.x, point.y) == Hit::None
        {
            return HTCAPTION as LRESULT;
        }
        HTCLIENT as LRESULT
    }
}

unsafe fn paint_toolbar(ui: &mut NativeUi, dc: HDC) {
    unsafe {
        let (width, _) = client_size(ui.window);
        let memory = CreateCompatibleDC(dc);
        let bitmap = CreateCompatibleBitmap(dc, width, TOOLBAR_HEIGHT);
        let old_bitmap = SelectObject(memory, bitmap as HGDIOBJ);
        let rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: TOOLBAR_HEIGHT,
        };
        FillRect(memory, &rect, ui.background_brush);
        SetBkMode(memory, TRANSPARENT as i32);
        DrawIconEx(
            memory,
            15,
            8,
            ui.logo_icon,
            28,
            28,
            0,
            ptr::null_mut(),
            DI_NORMAL,
        );
        for visual in &ui.tabs {
            draw_tab(ui, memory, visual);
        }
        draw_button(
            memory,
            ui,
            Hit::NewTab,
            RECT {
                left: width - 176,
                top: 7,
                right: width - 142,
                bottom: 37,
            },
            "+",
            true,
        );
        draw_button(
            memory,
            ui,
            Hit::Back,
            RECT {
                left: 14,
                top: 59,
                right: 48,
                bottom: 95,
            },
            "←",
            ui.can_go_back,
        );
        draw_button(
            memory,
            ui,
            Hit::Forward,
            RECT {
                left: 56,
                top: 59,
                right: 90,
                bottom: 95,
            },
            "→",
            ui.can_go_forward,
        );
        draw_button(
            memory,
            ui,
            Hit::Reload,
            RECT {
                left: 98,
                top: 59,
                right: 132,
                bottom: 95,
            },
            if ui.is_loading { "×" } else { "↻" },
            true,
        );
        draw_button(
            memory,
            ui,
            Hit::Home,
            RECT {
                left: 140,
                top: 59,
                right: 174,
                bottom: 95,
            },
            "⌂",
            true,
        );
        draw_button(
            memory,
            ui,
            Hit::Downloads,
            RECT {
                left: width - 100,
                top: 59,
                right: width - 58,
                bottom: 95,
            },
            "↓",
            true,
        );
        draw_button(
            memory,
            ui,
            Hit::Menu,
            RECT {
                left: width - 50,
                top: 59,
                right: width - 14,
                bottom: 95,
            },
            "⋮",
            true,
        );
        draw_button(
            memory,
            ui,
            Hit::Minimize,
            RECT {
                left: width - 138,
                top: 0,
                right: width - 92,
                bottom: TAB_HEIGHT,
            },
            "—",
            true,
        );
        draw_button(
            memory,
            ui,
            Hit::Maximize,
            RECT {
                left: width - 92,
                top: 0,
                right: width - 46,
                bottom: TAB_HEIGHT,
            },
            if IsZoomed(ui.window) != 0 {
                "❐"
            } else {
                "□"
            },
            true,
        );
        draw_caption_close(ui, memory, width);
        let divider = RECT {
            left: 0,
            top: TOOLBAR_HEIGHT - 1,
            right: width,
            bottom: TOOLBAR_HEIGHT,
        };
        let brush = CreateSolidBrush(DIVIDER);
        FillRect(memory, &divider, brush);
        DeleteObject(brush as HGDIOBJ);
        BitBlt(dc, 0, 0, width, TOOLBAR_HEIGHT, memory, 0, 0, SRCCOPY);
        SelectObject(memory, old_bitmap);
        DeleteObject(bitmap as HGDIOBJ);
        DeleteDC(memory);
    }
}

unsafe fn draw_tab(ui: &NativeUi, dc: HDC, visual: &TabVisual) {
    unsafe {
        let x = visual.x.value.round() as i32;
        let rect = RECT {
            left: x,
            top: 5,
            right: x + visual.width.round() as i32,
            bottom: 40,
        };
        let active = visual.tab.active;
        let hovered =
            ui.hover == Hit::Tab(visual.tab.id) || ui.hover == Hit::TabClose(visual.tab.id);
        if active || hovered {
            fill_round_rect(dc, &rect, if active { SURFACE } else { HOVER }, 12, None);
        }
        SetBkMode(dc, TRANSPARENT as i32);
        if visual.tab.pinned {
            let initial = visual.tab.title.chars().next().unwrap_or('•').to_string();
            draw_text(
                dc,
                &initial,
                &rect,
                ui.title_font,
                if active { ACCENT } else { SECONDARY },
                DT_CENTER,
            );
            return;
        }
        let mut title_rect = rect;
        title_rect.left += 13;
        title_rect.right -= 29;
        draw_text(
            dc,
            &visual.tab.title,
            &title_rect,
            ui.title_font,
            if active { FOREGROUND } else { SECONDARY },
            DT_LEFT | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        if visual.tab.loading {
            let indicator = RECT {
                left: rect.left + 7,
                top: rect.top + 5,
                right: rect.left + 18,
                bottom: rect.top + 17,
            };
            draw_text(dc, "•", &indicator, ui.small_font, ACCENT, DT_CENTER);
        }
        let close = RECT {
            left: rect.right - 29,
            top: 11,
            right: rect.right - 5,
            bottom: 35,
        };
        if ui.hover == Hit::TabClose(visual.tab.id) {
            fill_round_rect(dc, &close, HOVER, 8, None);
        }
        draw_text(dc, "×", &close, ui.icon_font, SECONDARY, DT_CENTER);
    }
}

unsafe fn draw_button(dc: HDC, ui: &NativeUi, hit: Hit, rect: RECT, text: &str, enabled: bool) {
    unsafe {
        if ui.hover == hit || ui.pressed == hit {
            fill_round_rect(dc, &rect, HOVER, 11, None);
        }
        draw_text(
            dc,
            text,
            &rect,
            ui.icon_font,
            if enabled { FOREGROUND } else { SECONDARY },
            DT_CENTER,
        );
    }
}

unsafe fn draw_caption_close(ui: &NativeUi, dc: HDC, width: i32) {
    unsafe {
        let rect = RECT {
            left: width - 46,
            top: 0,
            right: width,
            bottom: TAB_HEIGHT,
        };
        if ui.hover == Hit::CloseWindow || ui.pressed == Hit::CloseWindow {
            let brush = CreateSolidBrush(CLOSE_HOVER);
            FillRect(dc, &rect, brush);
            DeleteObject(brush as HGDIOBJ);
        }
        draw_text(
            dc,
            "×",
            &rect,
            ui.icon_font,
            if ui.hover == Hit::CloseWindow {
                SURFACE
            } else {
                FOREGROUND
            },
            DT_CENTER,
        );
    }
}

unsafe fn draw_download_panel(ui: &NativeUi, dc: HDC) {
    unsafe {
        let mut client = RECT::default();
        GetClientRect(ui.downloads_window, &mut client);
        FillRect(dc, &client, ui.background_brush);
        fill_round_rect(dc, &client, SURFACE, 16, Some(DIVIDER));
        SetBkMode(dc, TRANSPARENT as i32);
        let title = RECT {
            left: 20,
            top: 8,
            right: client.right - 52,
            bottom: 45,
        };
        draw_text(
            dc,
            "Загрузки",
            &title,
            ui.font,
            FOREGROUND,
            DT_LEFT | DT_VCENTER,
        );
        let close = RECT {
            left: client.right - 46,
            top: 8,
            right: client.right - 10,
            bottom: 44,
        };
        draw_text(dc, "×", &close, ui.icon_font, SECONDARY, DT_CENTER);
        if ui.downloads.is_empty() {
            let empty = RECT {
                left: 20,
                top: 50,
                right: client.right - 20,
                bottom: 86,
            };
            draw_text(
                dc,
                "Загрузок пока нет",
                &empty,
                ui.title_font,
                SECONDARY,
                DT_LEFT | DT_VCENTER,
            );
            return;
        }
        for (index, download) in ui.downloads.iter().enumerate() {
            let top = 54 + index as i32 * 74;
            let name = RECT {
                left: 18,
                top,
                right: client.right - 132,
                bottom: top + 30,
            };
            draw_text(
                dc,
                &download.name,
                &name,
                ui.title_font,
                FOREGROUND,
                DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            let status = match download.status {
                DownloadStatus::InProgress => format!("{}%", download.percent.clamp(0, 100)),
                DownloadStatus::Paused => "Пауза".to_owned(),
                DownloadStatus::Complete => format_bytes(download.received.max(download.total)),
                DownloadStatus::Canceled => "Отменено".to_owned(),
                DownloadStatus::Interrupted => "Загрузка прервана".to_owned(),
            };
            let info = RECT {
                left: 18,
                top: top + 30,
                right: client.right - 132,
                bottom: top + 57,
            };
            draw_text(
                dc,
                &status,
                &info,
                ui.small_font,
                SECONDARY,
                DT_LEFT | DT_VCENTER,
            );
            let action = RECT {
                left: client.right - 126,
                top: top + 17,
                right: client.right - 62,
                bottom: top + 52,
            };
            match download.status {
                DownloadStatus::Complete => {
                    draw_text(dc, "Открыть", &action, ui.small_font, ACCENT, DT_CENTER)
                }
                DownloadStatus::InProgress | DownloadStatus::Paused => draw_text(
                    dc,
                    if download.status == DownloadStatus::Paused {
                        "▶"
                    } else {
                        "Ⅱ"
                    },
                    &action,
                    ui.icon_font,
                    FOREGROUND,
                    DT_CENTER,
                ),
                _ => {}
            }
            if matches!(
                download.status,
                DownloadStatus::InProgress | DownloadStatus::Paused
            ) {
                let cancel = RECT {
                    left: client.right - 56,
                    top: top + 17,
                    right: client.right - 14,
                    bottom: top + 52,
                };
                draw_text(dc, "×", &cancel, ui.icon_font, SECONDARY, DT_CENTER);
            }
            if matches!(
                download.status,
                DownloadStatus::InProgress | DownloadStatus::Paused
            ) {
                let progress = RECT {
                    left: 18,
                    top: top + 62,
                    right: client.right - 18,
                    bottom: top + 65,
                };
                fill_round_rect(dc, &progress, DIVIDER, 2, None);
                let mut filled = progress;
                filled.right = filled.left
                    + (progress.right - progress.left) * download.percent.clamp(0, 100) / 100;
                if filled.right > filled.left {
                    fill_round_rect(dc, &filled, ACCENT, 2, None);
                }
            }
        }
    }
}

unsafe fn fill_round_rect(
    dc: HDC,
    rect: &RECT,
    color: COLORREF,
    radius: i32,
    border: Option<COLORREF>,
) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let pen = CreatePen(PS_SOLID, 1, border.unwrap_or(color));
        let old_brush = SelectObject(dc, brush as HGDIOBJ);
        let old_pen = SelectObject(dc, pen as HGDIOBJ);
        RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        DeleteObject(brush as HGDIOBJ);
        DeleteObject(pen as HGDIOBJ);
    }
}

unsafe fn draw_text(dc: HDC, text: &str, rect: &RECT, font: HFONT, color: COLORREF, flags: u32) {
    unsafe {
        SelectObject(dc, font as HGDIOBJ);
        SetTextColor(dc, color);
        SetBkMode(dc, TRANSPARENT as i32);
        let mut text = wide(text);
        let mut rect = *rect;
        DrawTextW(
            dc,
            text.as_mut_ptr(),
            text.len() as i32 - 1,
            &mut rect,
            flags | DT_SINGLELINE,
        );
    }
}

fn format_bytes(bytes: i64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} МБ", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} КБ", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} Б")
    }
}

unsafe fn show_main_menu(window: HWND) {
    unsafe {
        let Some(ui) = ui_mut(window) else { return };
        let controller = ui.controller.clone();
        let preferences = lock_controller(&controller).preferences();
        let menu = CreatePopupMenu();
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_PRIVATE,
            wide("Новое приватное окно\tCtrl+Shift+N").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_RESTORE,
            wide("Восстановить закрытую вкладку\tCtrl+Shift+T").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            menu,
            MF_STRING
                | if preferences.save_history {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                },
            MENU_HISTORY,
            wide("Сохранять историю").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING
                | if preferences.ask_download_path {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                },
            MENU_DOWNLOAD_PROMPT,
            wide("Спрашивать путь загрузки").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_CLEAR_HOUR,
            wide("Очистить историю за час").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_CLEAR_DAY,
            wide("Очистить историю за сутки").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_CLEAR_ALL,
            wide("Очистить всю историю").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_CLEAR_SITE,
            wide("Очистить cookies, кэш и авторизацию").as_ptr(),
        );
        let point = cursor_position();
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            point.x,
            point.y,
            0,
            window,
            ptr::null(),
        );
        DestroyMenu(menu);
        match command {
            value if value == MENU_PRIVATE as i32 => BrowserController::open_private_window(),
            value if value == MENU_RESTORE as i32 => {
                BrowserController::restore_closed_tab(&controller)
            }
            value if value == MENU_HISTORY as i32 => BrowserController::toggle_history(&controller),
            value if value == MENU_DOWNLOAD_PROMPT as i32 => {
                BrowserController::toggle_download_prompt(&controller)
            }
            value if value == MENU_CLEAR_HOUR as i32 => {
                BrowserController::clear_history(Some(3600))
            }
            value if value == MENU_CLEAR_DAY as i32 => {
                BrowserController::clear_history(Some(86_400))
            }
            value if value == MENU_CLEAR_ALL as i32 => BrowserController::clear_history(None),
            value
                if value == MENU_CLEAR_SITE as i32
                    && MessageBoxW(
                        window,
                        wide("Удалить cookies, кэш и сохранённые авторизации для всех сайтов?")
                            .as_ptr(),
                        wide("KOCKERS — очистка данных").as_ptr(),
                        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                    ) == IDYES =>
            {
                BrowserController::clear_site_data();
            }
            _ => {}
        }
    }
}

unsafe fn show_tab_menu(window: HWND, id: u64) {
    unsafe {
        let Some(ui) = ui_mut(window) else { return };
        let controller = ui.controller.clone();
        let menu = CreatePopupMenu();
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_PIN,
            wide("Закрепить или открепить").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_CLOSE_TAB,
            wide("Закрыть вкладку").as_ptr(),
        );
        let point = cursor_position();
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            point.x,
            point.y,
            0,
            window,
            ptr::null(),
        );
        DestroyMenu(menu);
        if command == MENU_PIN as i32 {
            BrowserController::toggle_pin(&controller, id);
        } else if command == MENU_CLOSE_TAB as i32 {
            BrowserController::close_tab(&controller, id);
        }
    }
}

unsafe fn watch_outside_clicks(owner: HWND) {
    unsafe {
        let previous = DOWNLOADS_OWNER.swap(owner as usize, Ordering::SeqCst);
        if previous != 0 && previous != owner as usize {
            PostMessageW(previous as HWND, WM_CLOSE_DOWNLOADS, 0, 0);
        }
        if DOWNLOADS_HOOK.load(Ordering::SeqCst) == 0 {
            let hook = SetWindowsHookExW(
                WH_MOUSE,
                Some(outside_click_hook),
                ptr::null_mut(),
                GetCurrentThreadId(),
            );
            DOWNLOADS_HOOK.store(hook as usize, Ordering::SeqCst);
        }
    }
}

unsafe fn stop_watching_outside_clicks(owner: HWND) {
    unsafe {
        if DOWNLOADS_OWNER
            .compare_exchange(owner as usize, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let hook = DOWNLOADS_HOOK.swap(0, Ordering::SeqCst);
            if hook != 0 {
                UnhookWindowsHookEx(hook as HHOOK);
            }
        }
    }
}

unsafe extern "system" fn outside_click_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0
            && matches!(
                wparam as u32,
                WM_LBUTTONDOWN
                    | WM_RBUTTONDOWN
                    | WM_MBUTTONDOWN
                    | WM_NCLBUTTONDOWN
                    | WM_NCRBUTTONDOWN
                    | WM_NCMBUTTONDOWN
            )
        {
            let owner = DOWNLOADS_OWNER.load(Ordering::SeqCst) as HWND;
            let info = &*(lparam as *const MOUSEHOOKSTRUCT);
            if !owner.is_null()
                && let Some(ui) = ui_mut(owner)
                && clicked_outside_downloads(ui, info)
            {
                PostMessageW(owner, WM_CLOSE_DOWNLOADS, 0, 0);
            }
        }
        CallNextHookEx(ptr::null_mut(), code, wparam, lparam)
    }
}

unsafe fn clicked_outside_downloads(ui: &NativeUi, info: &MOUSEHOOKSTRUCT) -> bool {
    unsafe {
        if info.hwnd == ui.downloads_window || IsChild(ui.downloads_window, info.hwnd) != 0 {
            return false;
        }
        if info.hwnd == ui.window {
            let mut point = info.pt;
            ScreenToClient(ui.window, &mut point);
            return ui.hit_test(point.x, point.y) != Hit::Downloads;
        }
        true
    }
}

fn cursor_position() -> POINT {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) };
    point
}

unsafe fn track_mouse_leave(window: HWND) {
    unsafe {
        let mut track = TRACKMOUSEEVENT {
            cbSize: mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: window,
            dwHoverTime: 0,
        };
        TrackMouseEvent(&mut track);
    }
}

unsafe fn create_child(
    class: &str,
    text: &str,
    style: WINDOW_STYLE,
    instance: HINSTANCE,
    parent: HWND,
    id: usize,
) -> Result<HWND, String> {
    unsafe {
        let window = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            style,
            0,
            0,
            0,
            0,
            parent,
            id as HMENU,
            instance,
            ptr::null(),
        );
        if window.is_null() {
            Err(format_windows_error(&format!(
                "Не удалось создать элемент управления {class}"
            )))
        } else {
            Ok(window)
        }
    }
}

unsafe fn create_font(height: i32, family: &str) -> HFONT {
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.into(),
            OUT_DEFAULT_PRECIS.into(),
            CLIP_DEFAULT_PRECIS.into(),
            CLEARTYPE_QUALITY.into(),
            (DEFAULT_PITCH | FF_DONTCARE).into(),
            wide(family).as_ptr(),
        )
    }
}

fn format_windows_error(prefix: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{prefix} (код Windows {code}).")
}

fn set_window_text(window: HWND, text: &str) {
    unsafe { SetWindowTextW(window, wide(text).as_ptr()) };
}

fn window_text(window: HWND) -> String {
    unsafe {
        let length = SendMessageW(window, WM_GETTEXTLENGTH, 0, 0);
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = SendMessageW(
            window,
            WM_GETTEXT,
            buffer.len(),
            buffer.as_mut_ptr() as isize,
        );
        if copied <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..copied as usize])
    }
}

fn ui_ptr(window: HWND) -> *mut NativeUi {
    unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) as *mut NativeUi }
}

fn ui_mut(window: HWND) -> Option<&'static mut NativeUi> {
    let pointer = ui_ptr(window);
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { &mut *pointer })
    }
}

fn key_down(key: i32) -> bool {
    unsafe { (GetKeyState(key) as u16 & 0x8000) != 0 }
}

const fn low_word(value: LPARAM) -> i16 {
    (value as usize & 0xFFFF) as u16 as i16
}

const fn high_word(value: LPARAM) -> i16 {
    ((value as usize >> 16) & 0xFFFF) as u16 as i16
}

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    red as COLORREF | ((green as COLORREF) << 8) | ((blue as COLORREF) << 16)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_overshoots_and_settles_without_permanent_timer_work() {
        let mut spring = Spring::new(30.0);
        spring.target = 0.0;
        let mut overshot = false;
        for _ in 0..500 {
            spring.step(0.016);
            overshot |= spring.value < 0.0;
        }
        assert!(overshot);
        assert_eq!(spring.value, 0.0);
        assert_eq!(spring.velocity, 0.0);
    }
}
