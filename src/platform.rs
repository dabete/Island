// OS-specific code lives here so the rest of the app stays portable.
use iced::Size;

/// Makes only a rectangle of the window clickable, so the transparent margin around the pill
/// no longer blocks clicks on other apps. All values are logical pixels: the rectangle is
/// `width` wide, centred horizontally in a window `window_size` wide, from `top` to `bottom`.
#[cfg(windows)]
pub fn set_clickable_area(window: &dyn iced::Window, window_size: Size, width: f32, top: f32, bottom: f32) {
    use windows::Win32::Graphics::Gdi::{CreateRectRgn, SetWindowRgn};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::Shell::SetWindowSubclass;
    use windows::Win32::UI::WindowsAndMessaging::{GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_NOACTIVATE};

    let Some(hwnd) = hwnd_of(window) else { return };

    // Regions are in physical pixels; our sizes are logical.
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let left = ((window_size.width - width) / 2.0 * scale).floor() as i32;
    let right = ((window_size.width + width) / 2.0 * scale).ceil() as i32;
    let (top, bottom) = ((top * scale).floor() as i32, (bottom * scale).ceil() as i32);

    // A plain rectangle, not a rounded one: a rounded region would clip the pill's smooth edge.
    // Once SetWindowRgn succeeds Windows owns the region, so we don't free it.
    unsafe {
        // Clicking a window normally makes it the active one and takes keyboard focus away from
        // what you were typing in. This style stops that; the clicks themselves still arrive.
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if style & WS_EX_NOACTIVATE.0 as isize == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_NOACTIVATE.0 as isize);
        }
        // Installing the same subclass again is harmless, so this runs on every call.
        let _ = SetWindowSubclass(hwnd, Some(no_frame_paint), 1, 0);
        let region = CreateRectRgn(left, top, right, bottom);
        SetWindowRgn(hwnd, Some(region), true);
    }
}

/// Hides or shows the window without taking keyboard focus when it comes back. A hidden window
/// can't block clicks and doesn't make Windows compose a fullscreen game the slow way.
#[cfg(windows)]
pub fn set_visible(window: &dyn iced::Window, visible: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow};

    if let Some(hwnd) = hwnd_of(window) {
        let _ = unsafe { ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE }) };
    }
}

#[cfg(windows)]
fn hwnd_of(window: &dyn iced::Window) -> Option<windows::Win32::Foundation::HWND> {
    use raw_window_handle::RawWindowHandle;
    use windows::Win32::Foundation::HWND;

    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut _)),
        _ => None,
    }
}

// With a window region, Windows paints a title-bar-coloured box into the pill whenever the window
// loses focus (e.g. you click another app). This intercepts those repaint messages.
#[cfg(windows)]
unsafe extern "system" fn no_frame_paint(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _id: usize,
    _data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::{LPARAM, LRESULT};
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{WM_NCACTIVATE, WM_NCPAINT};
    match msg {
        WM_NCPAINT => LRESULT(0),
        // lParam -1 tells Windows not to repaint the frame when activation changes.
        WM_NCACTIVATE => unsafe { DefSubclassProc(hwnd, msg, wparam, LPARAM(-1)) },
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(not(windows))]
pub fn set_clickable_area(_window: &dyn iced::Window, _window_size: Size, _width: f32, _top: f32, _bottom: f32) {}

#[cfg(not(windows))]
pub fn set_visible(_window: &dyn iced::Window, _visible: bool) {}
