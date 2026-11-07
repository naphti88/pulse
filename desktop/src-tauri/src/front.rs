// Whether the Claude app is the one in front: an answer that comes while it
// is not has no reader, whatever session the app has on show.

#[cfg(target_os = "macos")]
use objc2_app_kit::NSWorkspace;

#[cfg(target_os = "macos")]
const CLAUDE: &str = "com.anthropic.claudefordesktop";

#[cfg(target_os = "macos")]
pub fn claude_in_front() -> bool {
    let front = NSWorkspace::sharedWorkspace().frontmostApplication();
    front.and_then(|app| app.bundleIdentifier()).is_some_and(|id| id.to_string() == CLAUDE)
}

// on Windows the app is ...\app\claude.exe in its package; Claude Code's own
// claude.exe in a terminal is not the app
#[cfg(windows)]
pub fn claude_in_front() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    let mut pid = 0u32;
    // SAFETY: plain queries; the handle is closed before returning
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() || GetWindowThreadProcessId(window, &mut pid) == 0 || pid == 0 {
            return false;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(process);
        ok && is_app(&String::from_utf16_lossy(&buf[..len as usize]))
    }
}

#[cfg(windows)]
fn is_app(path: &str) -> bool {
    let path = std::path::Path::new(path);
    let named = |p: Option<&std::ffi::OsStr>, name: &str| p.and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(name));
    named(path.file_name(), "claude.exe") && named(path.parent().and_then(|p| p.file_name()), "app")
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn claude_in_front() -> bool {
    false
}

#[cfg(all(test, windows))]
mod tests {
    use super::is_app;

    #[test]
    fn only_the_app_counts_not_claude_code() {
        assert!(is_app(r"C:\Program Files\WindowsApps\Claude_4.23118.0.0_x64__pzs8sxrjxfjjc\app\claude.exe"));
        assert!(is_app(r"C:\Program Files\WindowsApps\Claude_1.0.0.0_x64__x\APP\Claude.exe"));
        assert!(!is_app(r"C:\Users\someone\.local\bin\claude.exe"));
        assert!(!is_app(r"C:\Program Files\WindowsApps\Claude_1.0.0.0_x64__x\app\chrome-native-host.exe"));
    }
}
