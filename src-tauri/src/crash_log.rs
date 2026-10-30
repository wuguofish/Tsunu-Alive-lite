//! 當機紀錄：把 panic 與記憶體配置失敗寫進 `<LocalAppData>/com.atone.tsunu-alive-lite/logs/crash.log`。
//!
//! release 版沒有主控台，Rust 在中止前印到 stderr 的訊息會直接消失；
//! 兩種中止（panic、配置失敗）在 Windows 都只留下同一個例外代碼 0xc0000409，
//! 沒有這份紀錄就分不出是哪一種、倒在哪一行。

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::OnceLock;

static LOG_FILE: OnceLock<File> = OnceLock::new();

/// 開好紀錄檔並掛上 panic hook。要在建立 Tauri app 之前呼叫，才接得到啟動階段的 panic。
pub fn init() {
    if let Some(file) = open_log_file() {
        let _ = LOG_FILE.set(file);
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let entry = format!(
            "{} panic on thread '{}': {}\n{}\n\n",
            chrono::Local::now().to_rfc3339(),
            thread.name().unwrap_or("<unnamed>"),
            info,
            std::backtrace::Backtrace::force_capture(),
        );
        write_log(entry.as_bytes());
        default_hook(info);
    }));
}

fn open_log_file() -> Option<File> {
    let dir = dirs::data_local_dir()?.join("com.atone.tsunu-alive-lite").join("logs");
    fs::create_dir_all(&dir).ok()?;
    OpenOptions::new().create(true).append(true).open(dir.join("crash.log")).ok()
}

fn write_log(bytes: &[u8]) {
    if let Some(mut file) = LOG_FILE.get() {
        let _ = file.write_all(bytes);
        let _ = file.flush();
    }
}

/// 包住系統配置器，配置失敗時先記下要配置的大小再交回 null（之後 Rust 照常中止）。
/// 這條路徑本身不能再配置記憶體，所以訊息在堆疊上的固定緩衝區裡組好。
pub struct LoggingAllocator;

unsafe impl GlobalAlloc for LoggingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_failure(System.alloc(layout), layout.size())
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_failure(System.alloc_zeroed(layout), layout.size())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_failure(System.realloc(ptr, layout, new_size), new_size)
    }
}

fn record_failure(ptr: *mut u8, size: usize) -> *mut u8 {
    if ptr.is_null() {
        let mut buf = [0u8; 64];
        let len = format_alloc_failure(size, &mut buf);
        write_log(&buf[..len]);
    }
    ptr
}

/// 把「memory allocation of N bytes failed」寫進 buf，回傳長度。不配置任何記憶體。
fn format_alloc_failure(size: usize, buf: &mut [u8; 64]) -> usize {
    const PREFIX: &[u8] = b"memory allocation of ";
    const SUFFIX: &[u8] = b" bytes failed\n";

    let mut digits = [0u8; 20];
    let mut n = size;
    let mut count = 0;
    loop {
        digits[19 - count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }

    let mut len = 0;
    for part in [PREFIX, &digits[20 - count..], SUFFIX] {
        buf[len..len + part.len()].copy_from_slice(part);
        len += part.len();
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_failure_message_has_exact_size() {
        let mut buf = [0u8; 64];
        let len = format_alloc_failure(1_270_000, &mut buf);
        assert_eq!(&buf[..len], b"memory allocation of 1270000 bytes failed\n");
    }

    #[test]
    fn alloc_failure_message_fits_largest_size() {
        let mut buf = [0u8; 64];
        let len = format_alloc_failure(usize::MAX, &mut buf);
        assert!(std::str::from_utf8(&buf[..len]).unwrap().contains(&usize::MAX.to_string()));
    }

    #[test]
    fn alloc_failure_message_for_zero() {
        let mut buf = [0u8; 64];
        let len = format_alloc_failure(0, &mut buf);
        assert_eq!(&buf[..len], b"memory allocation of 0 bytes failed\n");
    }
}
