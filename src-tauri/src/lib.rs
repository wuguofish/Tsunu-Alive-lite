use serde_json::{json, Value};
use std::path::PathBuf;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

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

            // Avatar 狀態
            if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
                for item in content {
                    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match item_type {
                        "thinking" => { let _ = app.emit("avatar-state", "thinking"); }
                        "tool_use" => { let _ = app.emit("avatar-state", "working"); }
                        "text" => { let _ = app.emit("avatar-state", "idle"); }
                        _ => {}
                    }
                }
            }

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
        "user" => {
            if let Some(arr) = json.get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for item in arr {
                    if item.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                        let _ = app.emit("avatar-state", "working");
                        return;
                    }
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

/// 啟動 JSONL watcher（背景輪詢）
/// session_id: 已知的 session ID（續接對話時有），None 表示新對話（等新檔案出現）
#[tauri::command]
async fn start_jsonl_watcher(
    app: AppHandle,
    working_dir: String,
    session_id: Option<String>,
) -> Result<(), String> {
    // 停止之前的 watcher
    if let Some(flag) = app.try_state::<Arc<AtomicBool>>() {
        flag.store(true, Ordering::Relaxed);
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    app.manage(stop_flag.clone());

    let app_clone = app.clone();
    let flag = stop_flag.clone();

    std::thread::spawn(move || {
        let mut target_file: Option<PathBuf> = None;
        let mut last_offset: u64 = 0;

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
                                eprintln!("👀 New session JSONL detected: {:?}", path.file_name());
                                break;
                            }
                        }
                    }
                }

                // 讀取目標檔案的新內容
                if let Some(ref path) = target_file {
                    let current_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    if current_size > last_offset {
                        if let Ok(content) = fs::read_to_string(path) {
                            if let Some(new_content) = content.get(last_offset as usize..) {
                                for line in new_content.lines() {
                                    if line.trim().is_empty() { continue; }
                                    process_jsonl_line(line, &app_clone);
                                }
                            }
                        }
                        last_offset = current_size;
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
    if let Some(flag) = app.try_state::<Arc<AtomicBool>>() {
        flag.store(true, Ordering::Relaxed);
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

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_pty::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            load_sessions,
            start_jsonl_watcher,
            stop_jsonl_watcher,
            save_temp_image_png,
            cleanup_temp_image,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
