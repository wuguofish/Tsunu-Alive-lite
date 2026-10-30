mod crash_log;
mod mod_state;

#[global_allocator]
static GLOBAL: crash_log::LoggingAllocator = crash_log::LoggingAllocator;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

/// 管理 JSONL watcher 的停止旗標，允許安全替換而不重複呼叫 app.manage()
struct WatcherControl {
    stop_flag: Mutex<Option<Arc<AtomicBool>>>,
}

impl Default for WatcherControl {
    fn default() -> Self {
        WatcherControl { stop_flag: Mutex::new(None) }
    }
}
/// 工作目錄路徑轉 Claude 專案目錄名稱
fn working_dir_to_project_dir_name(working_dir: &str) -> String {
    let name: String = working_dir.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if name.len() <= 200 { name } else { name[..200].to_string() }
}

/// 取得 Claude CLI 的專案目錄路徑
fn get_claude_project_dir(working_dir: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let dir_name = working_dir_to_project_dir_name(working_dir);
    let project_dir = home.join(".claude").join("projects").join(dir_name);
    if project_dir.exists() { Some(project_dir) } else { None }
}

/// 載入 session 列表
#[tauri::command]
async fn load_sessions(working_dir: String) -> Result<Value, String> {
    let project_dir = match get_claude_project_dir(&working_dir) {
        Some(dir) => dir,
        None => return Ok(json!({ "sessions": [] })),
    };

    let mut sessions: Vec<Value> = Vec::new();

    if let Ok(entries) = fs::read_dir(&project_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };

            let modified = entry.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Utc> = t.into();
                    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                })
                .unwrap_or_default();

            let title = fs::File::open(&path)
                .ok()
                .and_then(|f| {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(f);
                    reader.lines()
                        .take(20)
                        .filter_map(|line| line.ok())
                        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
                        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("user"))
                        .and_then(|v| {
                            v.get("message")
                                .and_then(|m| m.get("content"))
                                .and_then(|c| {
                                    if let Some(s) = c.as_str() {
                                        Some(s.to_string())
                                    } else if let Some(arr) = c.as_array() {
                                        arr.iter()
                                            .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
                                            .and_then(|item| item.get("text").and_then(|t| t.as_str()))
                                            .map(|s| s.to_string())
                                    } else {
                                        None
                                    }
                                })
                        })
                })
                .unwrap_or_default();

            sessions.push(json!({
                "sessionId": stem,
                "title": title,
                "modified": modified,
            }));
        }
    }

    sessions.sort_by(|a, b| {
        let ma = a.get("modified").and_then(|v| v.as_str()).unwrap_or("");
        let mb = b.get("modified").and_then(|v| v.as_str()).unwrap_or("");
        mb.cmp(ma)
    });

    eprintln!("📂 Loaded {} sessions from .jsonl scan", sessions.len());
    Ok(json!({ "sessions": sessions }))
}

/// 解析 JSONL 行，發送對應事件到前端
fn process_jsonl_line(line: &str, app: &AppHandle) {
    let json: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    let record_type = match json.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return,
    };

    match record_type {
        "assistant" => {
            let message = match json.get("message") {
                Some(m) => m,
                None => return,
            };

            // Model 名稱
            if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
                let _ = app.emit("model-info", model.to_string());
            }

            // Context usage（從 assistant message 的 usage 欄位）
            if let Some(usage) = message.get("usage") {
                let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let cache_read = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let cache_create = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let total = input + output + cache_read + cache_create;

                if total > 0 {
                    let _ = app.emit("context-usage", json!({
                        "input": input,
                        "output": output,
                        "cacheRead": cache_read,
                        "cacheCreate": cache_create,
                        "total": total,
                    }));
                }
            }
        }
        // JSONL 不包含 result 類型記錄（那只在 stream-json stdout 中）
        // context usage 和 model 已從 assistant 記錄中讀取
        "system" => {
            // Session ID
            if let Some(sid) = json.get("sessionId").and_then(|s| s.as_str()) {
                let _ = app.emit("session-info", sid.to_string());
            }
        }
        _ => {}
    }
}

/// 決定這次讀到的 buf 裡要處理哪一段完整的行，以及 offset 要往前推多少。
///
/// 結尾沒有換行的半行留到下次讀（避免解析半截 JSON）。但如果整段都讀滿了還找不到換行，
/// 代表單行比一次讀取的上限還長，等下去只會每次重讀同一段；這時跳過這一行，
/// 用 `skipping` 記住「還在跳」，下一段讀到換行為止都丟掉。
fn split_complete_lines(
    buf: &[u8],
    chunk_full: bool,
    skipping: &mut bool,
) -> (std::ops::Range<usize>, usize) {
    let mut start = 0;
    if *skipping {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => {
                start = i + 1;
                *skipping = false;
            }
            None => return (0..0, buf.len()),
        }
    }
    match buf[start..].iter().rposition(|&b| b == b'\n') {
        Some(i) => (start..start + i + 1, start + i + 1),
        None if chunk_full && start == 0 => {
            *skipping = true;
            (0..0, buf.len())
        }
        None => (start..start, start),
    }
}

/// 啟動 JSONL watcher（背景輪詢）
/// session_id: 已知的 session ID（續接對話時有），None 表示新對話（等新檔案出現）
#[tauri::command]
async fn start_jsonl_watcher(
    app: AppHandle,
    working_dir: String,
    session_id: Option<String>,
) -> Result<(), String> {
    // 停止之前的 watcher，並原子地換上新的 flag
    // Why: app.manage() 對同一型別只能呼叫一次，多次呼叫會 panic 導致閃退；
    //      改用 Mutex<Option<...>> 包裝，只在 run() 初始化一次，之後更新內容
    let new_flag = Arc::new(AtomicBool::new(false));
    {
        let state = app.state::<WatcherControl>();
        let mut guard = state.stop_flag.lock().unwrap();
        if let Some(old_flag) = &*guard {
            old_flag.store(true, Ordering::Relaxed);
        }
        *guard = Some(new_flag.clone());
    }

    let app_clone = app.clone();
    let flag = new_flag.clone();

    std::thread::spawn(move || {
        let mut target_file: Option<PathBuf> = None;
        let mut last_offset: u64 = 0;
        let mut skipping_oversized_line = false;

        // 記錄啟動前已存在的檔案（用於偵測新檔案）
        let mut existing_files: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        if let Some(project_dir) = get_claude_project_dir(&working_dir) {
            if let Ok(entries) = fs::read_dir(&project_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                        existing_files.insert(path);
                    }
                }
            }

            // 如果有指定 session_id，直接鎖定該檔案
            if let Some(ref sid) = session_id {
                let path = project_dir.join(format!("{}.jsonl", sid));
                if path.exists() {
                    target_file = Some(path.clone());
                    last_offset = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    eprintln!("👀 JSONL watcher: locked to session {} (offset={})", sid, last_offset);
                }
            }
        }

        eprintln!("👀 JSONL watcher started: session_id={:?}, {} existing files",
            session_id, existing_files.len());

        loop {
            if flag.load(Ordering::Relaxed) {
                eprintln!("🛑 JSONL watcher stopped");
                break;
            }

            if let Some(project_dir) = get_claude_project_dir(&working_dir) {
                // 還沒鎖定檔案 → 偵測新檔案（新對話會建立新的 .jsonl）
                if target_file.is_none() {
                    if let Ok(entries) = fs::read_dir(&project_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().and_then(|x| x.to_str()) == Some("jsonl")
                                && !existing_files.contains(&path)
                            {
                                target_file = Some(path.clone());
                                last_offset = 0; // 新檔案，從頭讀
                                skipping_oversized_line = false;
                                eprintln!("👀 New session JSONL detected: {:?}", path.file_name());
                                break;
                            }
                        }
                    }
                }

                // 讀取目標檔案的新內容（seek-based、只 alloc 新增段）
                // Why: 原本 fs::read_to_string 每次 read 整個檔（MB 級）、系統 RAM 緊張時會
                //      觸發 Rust allocator abort（0xc0000409）、整個 Tauri main process 倒
                // 每次最多讀 4MB，避免單次大量輸出時 allocator abort
                const MAX_CHUNK: u64 = 4 * 1024 * 1024;
                if let Some(ref path) = target_file {
                    let current_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    if current_size > last_offset {
                        if let Ok(mut file) = fs::File::open(path) {
                            if file.seek(SeekFrom::Start(last_offset)).is_ok() {
                                let to_read = (current_size - last_offset).min(MAX_CHUNK) as usize;
                                let mut buf = Vec::with_capacity(to_read);
                                if file.take(to_read as u64).read_to_end(&mut buf).is_ok() {
                                    let chunk_full = buf.len() as u64 == MAX_CHUNK;
                                    let (lines, consumed) =
                                        split_complete_lines(&buf, chunk_full, &mut skipping_oversized_line);
                                    if let Ok(text) = std::str::from_utf8(&buf[lines]) {
                                        for line in text.lines() {
                                            if line.trim().is_empty() { continue; }
                                            process_jsonl_line(line, &app_clone);
                                        }
                                    }
                                    last_offset += consumed as u64;
                                }
                            }
                        }
                    } else if current_size < last_offset {
                        // 檔案被 truncate / rotate、reset offset 從頭讀
                        last_offset = 0;
                        skipping_oversized_line = false;
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });

    Ok(())
}

/// 停止 JSONL watcher
#[tauri::command]
async fn stop_jsonl_watcher(app: AppHandle) -> Result<(), String> {
    let state = app.state::<WatcherControl>();
    if let Ok(guard) = state.stop_flag.lock() {
        if let Some(flag) = &*guard {
            flag.store(true, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// 儲存剪貼簿圖片到暫存檔案
#[tauri::command]
fn save_temp_image_png(png_data: Vec<u8>) -> Result<String, String> {
    use std::io::Write;

    let temp_dir = std::env::temp_dir().join("tsunu_alive_lite");
    if !temp_dir.exists() {
        fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("Failed to create temp directory: {}", e))?;
    }

    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f");
    let filename = format!("clipboard_{}.png", timestamp);
    let file_path = temp_dir.join(&filename);

    let mut file = fs::File::create(&file_path)
        .map_err(|e| format!("Failed to create file: {}", e))?;
    file.write_all(&png_data)
        .map_err(|e| format!("Failed to write PNG data: {}", e))?;

    Ok(file_path.to_string_lossy().to_string())
}

/// 清理暫存圖片
#[tauri::command]
fn cleanup_temp_image(file_path: String) -> Result<(), String> {
    let path = PathBuf::from(&file_path);
    let temp_dir = std::env::temp_dir().join("tsunu_alive_lite");
    if !path.starts_with(&temp_dir) {
        return Err("Cannot delete file outside temp directory".to_string());
    }
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|e| format!("Failed to delete temp file: {}", e))?;
    }
    Ok(())
}

/// 檔案是否存在。
///
/// 目前只給 LINE channel 用：前端要先確認 `~/.claude/line-mcp.json` 在不在，
/// 才決定要不要把 `--mcp-config` 加進啟動參數——Claude Code 對於指向不存在檔案的
/// `--mcp-config` 是直接拒絕啟動（Invalid MCP configuration），不是忽略，
/// 所以沒有這道檢查的話，沒建過設定檔的機器一勾 LINE 就完全開不起來。
#[tauri::command]
fn file_exists(file_path: String) -> bool {
    PathBuf::from(&file_path).is_file()
}

/// 啟動本 app 時帶的命令列參數（不含執行檔路徑），讓腳本能直接指定要續接的 session。
#[tauri::command]
fn launch_args() -> Vec<String> {
    std::env::args().skip(1).collect()
}

pub fn run() {
    crash_log::init();
    tauri::Builder::default()
        .manage(WatcherControl::default())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_pty::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let state = mod_state::start(app.handle());
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_sessions,
            start_jsonl_watcher,
            stop_jsonl_watcher,
            save_temp_image_png,
            cleanup_temp_image,
            file_exists,
            launch_args,
            mod_state::mod_launch_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_trailing_partial_line_for_next_read() {
        let mut skipping = false;
        let (lines, consumed) = split_complete_lines(b"{\"a\":1}\n{\"b\"", false, &mut skipping);
        assert_eq!((lines, consumed), (0..8, 8));
        assert!(!skipping);
    }

    #[test]
    fn waits_when_short_read_has_no_newline() {
        let mut skipping = false;
        assert_eq!(split_complete_lines(b"{\"half", false, &mut skipping), (0..0, 0));
        assert!(!skipping);
    }

    #[test]
    fn skips_line_longer_than_a_full_chunk() {
        let mut skipping = false;
        assert_eq!(split_complete_lines(b"xxxxxxxx", true, &mut skipping), (0..0, 8));
        assert!(skipping);

        // 下一段還沒看到換行：整段丟掉
        assert_eq!(split_complete_lines(b"yyyy", true, &mut skipping), (0..0, 4));
        assert!(skipping);

        // 讀到超長行的結尾：換行之前丟掉，之後的完整行照常處理
        let (lines, consumed) = split_complete_lines(b"zz\n{\"ok\":1}\n{\"next", false, &mut skipping);
        assert_eq!((lines, consumed), (3..12, 12));
        assert!(!skipping);
    }

    #[test]
    fn after_skipping_waits_on_partial_line() {
        let mut skipping = true;
        assert_eq!(split_complete_lines(b"tail\n{\"half", true, &mut skipping), (5..5, 5));
        assert!(!skipping);
    }
}
