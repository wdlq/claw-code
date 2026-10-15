//! Windows 控制台 ANSI 能力探测与输出净化。
//!
//! claw 直接打印 ANSI 转义序列上色。Win10 的 conhost 在设置
//! `ENABLE_VIRTUAL_TERMINAL_PROCESSING` 后可以解析（VS Code 的 ConPTY 会
//! 自动替子进程设置，所以 Win10 的 VS Code 终端能正常显示颜色）；Win7 的
//! conhost 完全没有 VT 解析能力。启动时尝试启用 VT 并记录控制台能否渲染
//! ANSI；不能渲染时 `sanitize` 会剥掉转义序列，让用户看到干净的单色文本
//! 而不是 `[38;5;196m` 这类乱码。

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

pub static ANSI_RENDERING_OK: AtomicBool = AtomicBool::new(false);

pub fn ansi_rendering_ok() -> bool {
    ANSI_RENDERING_OK.load(Ordering::Relaxed)
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 Console API 本身就是 unsafe 接口
pub fn init() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) != 0
            && SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        {
            ANSI_RENDERING_OK.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(not(windows))]
pub fn init() {
    ANSI_RENDERING_OK.store(true, Ordering::Relaxed);
}

/// 控制台无法渲染 ANSI 时剥掉转义序列；能渲染时原样返回。
pub fn sanitize(input: &str) -> Cow<'_, str> {
    if ansi_rendering_ok() || !input.contains('\x1b') {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                // CSI: ESC [ ... final(0x40..=0x7e)
                b'[' => {
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    i = if j < bytes.len() { j + 1 } else { bytes.len() };
                    continue;
                }
                // OSC: ESC ] ... BEL 或 ESC \
                b']' => {
                    let mut j = i + 2;
                    while j < bytes.len() {
                        if bytes[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                // 未知转义：连同后一个字节一起丢弃
                _ => {
                    i += 2;
                    continue;
                }
            }
        }
        // 转义序列本身都是 ASCII，i 一定落在 UTF-8 字符边界上
        let ch = input[i..].chars().next().expect("valid char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::{ansi_rendering_ok, sanitize};

    #[test]
    fn sanitize_strips_csi_when_ansi_unavailable() {
        // 强制走剥离分支（测试进程未启用 VT）
        ANSI_RENDERING_OK.store(false, Ordering::Relaxed);
        assert_eq!(sanitize("\x1b[38;5;196m红\x1b[0m"), "红");
        assert_eq!(sanitize("\x1b[2m暗淡\x1b[0m"), "暗淡");
        assert_eq!(sanitize("\x1b[1A\x1b[2K"), "");
    }

    #[test]
    fn sanitize_passes_through_plain_text() {
        ANSI_RENDERING_OK.store(false, Ordering::Relaxed);
        assert_eq!(sanitize("普通文本 🦞"), "普通文本 🦞");
    }

    #[test]
    fn sanitize_respects_ansi_ok_flag() {
        ANSI_RENDERING_OK.store(true, Ordering::Relaxed);
        assert_eq!(sanitize("\x1b[1m粗\x1b[0m"), "\x1b[1m粗\x1b[0m");
        ANSI_RENDERING_OK.store(false, Ordering::Relaxed);
    }
}
