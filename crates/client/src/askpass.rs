//! The password and host-key dialog.
//!
//! ssh runs without a console, so it asks through `SSH_ASKPASS`: this same
//! exe, started with [`ENV`] set and the prompt as its argument. The answer
//! goes to stdout (read by ssh only); Cancel exits with 1.

#![allow(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, SetProcessDpiAwarenessContext,
    SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

/// Set in ssh's environment so the exe knows it runs as the askpass program.
pub const ENV: &str = "FEROSTREAM_ASKPASS";

const ID_EDIT: i32 = 100;
const CLASS: PCWSTR = w!("FeroStreamAskpass");

thread_local! {
    static RESULT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Shows the dialog for `prompt` and prints the answer.
pub fn run(prompt: &str) -> ExitCode {
    let lower = prompt.to_lowercase();
    let secret = ["password", "passphrase", "pin", "verification code", "otp"]
        .iter()
        .any(|w| lower.contains(w));
    match ask(prompt, secret) {
        Some(mut answer) => {
            let mut out = std::io::stdout();
            let ok = out.write_all(answer.as_bytes()).is_ok() && out.write_all(b"\n").is_ok();
            let _ = out.flush();
            // Do not leave the password lying around in this process.
            // SAFETY: zero bytes are valid UTF-8.
            unsafe { answer.as_bytes_mut().fill(0) };
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        None => ExitCode::FAILURE,
    }
}

fn ask(prompt: &str, secret: bool) -> Option<String> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None).ok()?.into();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc_),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                (windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 + 1) as isize as *mut _,
            ),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&class);

        let dpi = GetDpiForSystem();
        let s = |v: i32| v * dpi as i32 / 96;
        let font = message_font(dpi);
        let text = prompt.trim_end().replace('\n', "\r\n");
        let lines = text.lines().count().max(1) as i32;
        let (w, pad, line) = (s(440), s(12), s(18));
        let text_h = lines * line;
        let h = pad + text_h + pad + s(24) + pad + s(28) + pad;

        let mut frame = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        let style = WS_CAPTION | WS_SYSMENU | WS_POPUP;
        let _ = AdjustWindowRectEx(&mut frame, style, false, WS_EX_DLGMODALFRAME);
        let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top);
        let x = (GetSystemMetrics(SM_CXSCREEN) - fw) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - fh) / 3;
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
            CLASS,
            w!("FeroStream - SSH"),
            style,
            x,
            y,
            fw,
            fh,
            None,
            None,
            Some(instance),
            None,
        )
        .ok()?;
        let child = |class: PCWSTR,
                     text: &HSTRING,
                     style: WINDOW_STYLE,
                     ex: WINDOW_EX_STYLE,
                     x,
                     y,
                     cw,
                     ch,
                     id: i32| {
            let h = CreateWindowExW(
                ex,
                class,
                text,
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                cw,
                ch,
                Some(dlg),
                Some(HMENU(id as isize as *mut _)),
                Some(instance),
                None,
            );
            if let (Ok(h), Some(f)) = (&h, font) {
                SendMessageW(*h, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
            }
            h.ok()
        };
        child(
            w!("STATIC"),
            &HSTRING::from(text.as_str()),
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
            pad,
            pad,
            w - 2 * pad,
            text_h,
            -1,
        );
        let edit_style = WS_TABSTOP
            | WINDOW_STYLE(ES_AUTOHSCROLL as u32)
            | if secret {
                WINDOW_STYLE(ES_PASSWORD as u32)
            } else {
                WINDOW_STYLE(0)
            };
        let edit = child(
            w!("EDIT"),
            &HSTRING::new(),
            edit_style,
            WS_EX_CLIENTEDGE,
            pad,
            pad + text_h + pad,
            w - 2 * pad,
            s(24),
            ID_EDIT,
        );
        let by = h - pad - s(28);
        child(
            w!("BUTTON"),
            &HSTRING::from("OK"),
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            w - 2 * s(88) - pad - s(8),
            by,
            s(88),
            s(28),
            IDOK.0,
        );
        child(
            w!("BUTTON"),
            &HSTRING::from("Cancel"),
            WS_TABSTOP,
            WINDOW_EX_STYLE(0),
            w - s(88) - pad,
            by,
            s(88),
            s(28),
            IDCANCEL.0,
        );
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);
        if let Some(e) = edit {
            let _ = SetFocus(Some(e));
        }

        let mut msg = MSG::default();
        while RESULT.get().is_none() && GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let ok = RESULT.get() == Some(true);
        let answer = edit.and_then(|e| {
            let n = GetWindowTextLengthW(e);
            let mut buf = vec![0u16; n as usize + 1];
            let got = GetWindowTextW(e, &mut buf) as usize;
            let s = String::from_utf16(buf.get(..got).unwrap_or_default()).ok();
            buf.fill(0);
            s
        });
        let _ = DestroyWindow(dlg);
        if ok { answer } else { None }
    }
}

fn message_font(dpi: u32) -> Option<HFONT> {
    let mut m = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            m.cbSize,
            Some((&mut m as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        )
        .ok()?;
        Some(CreateFontIndirectW(&m.lfMessageFont))
    }
}

extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let finish = |ok: bool| {
        RESULT.set(Some(ok));
        unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)).ok() };
    };
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as i32;
            if id == IDOK.0 {
                finish(true);
            } else if id == IDCANCEL.0 {
                finish(false);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            finish(false);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
