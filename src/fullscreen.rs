// Is another app fullscreen on the main monitor? Windows tells us when the window in front
// changes or moves, so nothing polls.
use iced::futures::Stream;
use iced::stream;

/// A window or monitor rectangle in screen pixels: (left, top, right, bottom).
pub type Rect = (i32, i32, i32, i32);

/// Does `window` cover the whole of `monitor` without a title bar? A normal maximised window keeps
/// its title bar, so with an auto-hiding taskbar (when it covers the whole monitor too) it still
/// doesn't count. Fullscreen videos, games and slideshows have no title bar.
pub fn is_fullscreen(window: Rect, monitor: Rect, has_title_bar: bool) -> bool {
    !has_title_bar
        && window.0 <= monitor.0
        && window.1 <= monitor.1
        && window.2 >= monitor.2
        && window.3 >= monitor.3
}

/// A stream that yields `true` when an app goes fullscreen on the main monitor and `false` when
/// it stops being so.
pub fn watch() -> impl Stream<Item = bool> {
    stream::channel(8, async |sender| {
        #[cfg(windows)]
        std::thread::spawn(move || windows_impl::run(sender));
        #[cfg(not(windows))]
        let _ = sender;
        // Keeps the subscription alive; the listener thread does the work.
        std::future::pending::<()>().await;
    })
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    use iced::futures::channel::mpsc::Sender;
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
        MonitorFromWindow,
    };
    use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, GWL_STYLE, GetClassNameW,
        GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowRect, IsIconic, MSG, TranslateMessage,
        WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WS_CAPTION,
    };

    use super::is_fullscreen;

    // A Windows event callback is a plain function and cannot capture anything, so the channel
    // to the app and the last answer live here.
    static SENDER: OnceLock<Mutex<Sender<bool>>> = OnceLock::new();
    static LAST: AtomicBool = AtomicBool::new(false);

    pub fn run(sender: Sender<bool>) {
        let _ = SENDER.set(Mutex::new(sender));
        unsafe {
            // Windows calls `on_event` on this thread, from inside the message loop below.
            let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
            let _foreground =
                SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_event), 0, 0, flags);
            let _moved = SetWinEventHook(
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_LOCATIONCHANGE,
                None,
                Some(on_event),
                0,
                0,
                flags,
            );

            // The state at startup, in case an app is already fullscreen.
            check();

            // Sleeps here until Windows has an event for us.
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    unsafe extern "system" fn on_event(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        _id_child: i32,
        _thread: u32,
        _time: u32,
    ) {
        // Every window that moves anywhere reports here, so ignore all but the front window itself
        // (object id 0 is "the window", as opposed to its caret, cursor, scrollbars and so on).
        if event == EVENT_OBJECT_LOCATIONCHANGE && (id_object != 0 || hwnd != unsafe { GetForegroundWindow() }) {
            return;
        }
        check();
    }

    // Looks at the front window and tells the app only if the answer changed.
    fn check() {
        let now = foreground_is_fullscreen();
        if LAST.swap(now, Ordering::Relaxed) != now {
            if let Some(Ok(mut sender)) = SENDER.get().map(|sender| sender.lock()) {
                // shortcut: a full queue drops this update, 8 slots is plenty for human-speed changes.
                let _ = sender.try_send(now);
            }
        }
    }

    fn foreground_is_fullscreen() -> bool {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() || IsIconic(hwnd).as_bool() {
                return false;
            }

            // The desktop and the taskbar cover the screen too, but they are not fullscreen apps.
            let mut class = [0u16; 64];
            let length = GetClassNameW(hwnd, &mut class).max(0) as usize;
            if matches!(String::from_utf16_lossy(&class[..length]).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
                return false;
            }

            // Only the main monitor counts: an app fullscreen on a second screen changes nothing.
            let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            if MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) != primary {
                return false;
            }
            let mut monitor = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let mut window = RECT::default();
            if !GetMonitorInfoW(primary, &mut monitor).as_bool() || GetWindowRect(hwnd, &mut window).is_err() {
                return false;
            }

            let caption = WS_CAPTION.0 as isize;
            let has_title_bar = GetWindowLongPtrW(hwnd, GWL_STYLE) & caption == caption;
            let m = monitor.rcMonitor;
            is_fullscreen((window.left, window.top, window.right, window.bottom), (m.left, m.top, m.right, m.bottom), has_title_bar)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: Rect = (0, 0, 1920, 1200);

    #[test]
    fn a_window_over_the_whole_monitor_without_a_title_bar_is_fullscreen() {
        assert!(is_fullscreen(MONITOR, MONITOR, false));
        // Some fullscreen windows are a few pixels bigger than the monitor.
        assert!(is_fullscreen((-8, -8, 1928, 1208), MONITOR, false));
    }

    #[test]
    fn maximised_windows_and_small_windows_are_not() {
        // Maximised with an auto-hiding taskbar covers the monitor, but keeps its title bar.
        assert!(!is_fullscreen((-8, -8, 1928, 1208), MONITOR, true));
        // Maximised with a taskbar leaves room for it.
        assert!(!is_fullscreen((0, 0, 1920, 1160), MONITOR, false));
        assert!(!is_fullscreen((100, 100, 900, 700), MONITOR, false));
    }

    #[test]
    fn covering_a_different_monitor_does_not_count() {
        let second_screen: Rect = (1920, 0, 3840, 1200);
        assert!(!is_fullscreen(second_screen, MONITOR, false));
    }
}
