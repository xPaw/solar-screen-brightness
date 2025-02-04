use crate::controller::{BrightnessController, Message};
use crate::gui::UserEvent;
use egui_winit::winit::event_loop::{EventLoop, EventLoopProxy};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::sync_channel;
use std::thread::JoinHandle;
use win32_utils::error::{check_error, CheckError};
use windows::core::{w, PCWSTR};
use windows::Win32::Graphics::Gdi::MONITORINFOEXW;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONULL,
    },
    System::{LibraryLoader::GetModuleHandleW, RemoteDesktop::WTSRegisterSessionNotification},
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcA, DispatchMessageW, GetMessageW, GetWindowRect,
            PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SendMessageW, CW_USEDEFAULT,
            EVENT_SYSTEM_FOREGROUND, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WINEVENT_OUTOFCONTEXT,
            WM_APP, WM_DISPLAYCHANGE, WM_WTSSESSION_CHANGE, WNDCLASSW, WTS_SESSION_LOCK,
            WTS_SESSION_UNLOCK,
        },
    },
};

const EXIT_LOOP: u32 = WM_APP + 999;

pub struct EventWatcher {
    thread: Option<JoinHandle<()>>,
    hwnd: HWND,
}

static GLOBAL_WINDOW_DATA: AtomicPtr<WindowData> = AtomicPtr::new(std::ptr::null_mut());

impl EventWatcher {
    pub fn start(
        controller: &BrightnessController,
        main_loop: Option<&EventLoop<UserEvent>>,
    ) -> anyhow::Result<Self> {
        let brightness_sender = controller.sender.clone();
        let proxy = main_loop.map(|m| m.create_proxy());
        let (tx, rx) = sync_channel(0);

        let thread = std::thread::spawn(move || {
            let window_data = Box::new(WindowData {
                sender: brightness_sender,
                open_window_msg_code: register_open_window_message(),
                main_loop: proxy,
                is_foreground_window_fullscreen: false,
            });

            GLOBAL_WINDOW_DATA.store(Box::into_raw(window_data), Ordering::SeqCst);

            unsafe {
                // Create Window Class
                let instance = GetModuleHandleW(None).unwrap();
                let window_class = WNDCLASSW {
                    hInstance: instance.into(),
                    lpszClassName: w!("ssb_event_watcher"),
                    lpfnWndProc: Some(wndproc),
                    ..Default::default()
                };
                let atom = check_error(|| RegisterClassW(&window_class)).unwrap();

                // Create window
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    PCWSTR(atom as *const u16),
                    None,
                    WINDOW_STYLE::default(),
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    None,
                    None,
                    instance,
                    None,
                )
                .check_error()
                .unwrap();

                tx.send(hwnd).unwrap();

                // Register for Session Notifications
                WTSRegisterSessionNotification(hwnd, 0).unwrap();

                // Register for foreground window change event
                // TODO: Maybe also need to listen for EVENT_OBJECT_LOCATIONCHANGE
                let hook_handle: HWINEVENTHOOK = SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(win_event_hook_proc),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                );

                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).into() {
                    DispatchMessageW(&message);
                }

                UnhookWinEvent(hook_handle);
            }
            log::debug!("EventWatcher thread exiting");
        });

        let hwnd = rx.recv().unwrap();
        Ok(EventWatcher {
            thread: Some(thread),
            hwnd,
        })
    }
}

impl Drop for EventWatcher {
    fn drop(&mut self) {
        log::info!("Stopping EventWatcher");
        unsafe { check_error(|| SendMessageW(self.hwnd, EXIT_LOOP, None, None)).unwrap() };
        self.thread.take().unwrap().join().unwrap();
    }
}

struct WindowData {
    sender: mpsc::Sender<Message>,
    open_window_msg_code: u32,
    main_loop: Option<EventLoopProxy<UserEvent>>,
    is_foreground_window_fullscreen: bool,
}

unsafe extern "system" fn wndproc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let window_data_ptr = GLOBAL_WINDOW_DATA.load(Ordering::SeqCst);
    if !window_data_ptr.is_null() {
        let window_data = &mut *window_data_ptr;

        match message {
            WM_DISPLAYCHANGE => {
                log::info!("Detected possible display change (WM_DISPLAYCHANGE)");
                window_data
                    .sender
                    .send(Message::Refresh("WM_DISPLAYCHANGE"))
                    .unwrap();
            }
            EXIT_LOOP => {
                log::debug!("Received EXIT_LOOP message");
                PostQuitMessage(0);
            }
            WM_WTSSESSION_CHANGE => match wparam.0 as u32 {
                WTS_SESSION_LOCK => {
                    log::info!("Detected WTS_SESSION_LOCK");
                    window_data
                        .sender
                        .send(Message::Disable("WTS_SESSION_LOCK"))
                        .unwrap();
                }
                WTS_SESSION_UNLOCK => {
                    log::info!("Detected WTS_SESSION_UNLOCK");
                    window_data
                        .sender
                        .send(Message::Enable("WTS_SESSION_UNLOCK"))
                        .unwrap();
                }
                _ => {}
            },
            msg if msg == window_data.open_window_msg_code => {
                if let Some(event_loop) = &window_data.main_loop {
                    log::info!("Opening window due to external message");
                    event_loop
                        .send_event(UserEvent::OpenWindow("Broadcast Message"))
                        .unwrap();
                }
            }
            _ => {}
        }
    }
    DefWindowProcA(window, message, wparam, lparam)
}

unsafe extern "system" fn win_event_hook_proc(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    let mut window_rect = RECT::default();
    if !GetWindowRect(hwnd, &mut window_rect).is_ok() {
        return;
    }

    let hmonitor: HMONITOR = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
    if hmonitor.is_invalid() {
        return;
    }

    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let info_ptr = &mut info as *mut _ as *mut MONITORINFO;

    if !GetMonitorInfoW(hmonitor, info_ptr).as_bool() {
        return;
    }

    let is_fullscreen = window_rect.left == info.monitorInfo.rcMonitor.left
        && window_rect.top == info.monitorInfo.rcMonitor.top
        && window_rect.right == info.monitorInfo.rcMonitor.right
        && window_rect.bottom == info.monitorInfo.rcMonitor.bottom;

    // TODO: EnumDisplayDevicesW to get the actual device paths?
    let display_name = wchar_to_string(&info.szDevice);

    let window_data_ptr = GLOBAL_WINDOW_DATA.load(Ordering::SeqCst);
    if window_data_ptr.is_null() {
        return;
    }

    let window_data = &mut *window_data_ptr;

    // TODO: This is likely to be buggy on multi monitor setups when both monitors have fullscreen apps open
    if window_data.is_foreground_window_fullscreen == is_fullscreen {
        return;
    }

    log::debug!(
        "Fullscreen: {} -> {} on '{}'",
        window_data.is_foreground_window_fullscreen,
        is_fullscreen,
        display_name
    );

    window_data.is_foreground_window_fullscreen = is_fullscreen;

    if is_fullscreen {
        window_data
            .sender
            .send(Message::FullscreenAdd(display_name))
            .unwrap();
    } else {
        window_data
            .sender
            .send(Message::FullscreenRemove(display_name))
            .unwrap();
    }
}

pub fn register_open_window_message() -> u32 {
    unsafe {
        check_error(|| RegisterWindowMessageW(w!("solar-screen-brightness.open_window"))).unwrap()
    }
}

fn wchar_to_string(s: &[u16]) -> String {
    let end = s.iter().position(|&x| x == 0).unwrap_or(s.len());
    let truncated = &s[0..end];
    OsString::from_wide(truncated).to_string_lossy().into()
}
