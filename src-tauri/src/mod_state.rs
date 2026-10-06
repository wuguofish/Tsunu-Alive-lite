//! 從 tsunu-avatar mod 取得 session 狀態，取代原本從 JSONL 猜狀態的做法。
//!
//! 本 app 開一個只聽 127.0.0.1 的隨機埠，啟動 claude 時用環境變數 `TSUNU_STATE_URL` 告訴 mod
//! 把狀態送來這裡，收到就轉成 `avatar-state` 事件給前端。mod 本身跟著安裝檔打包（`tsunu-avatar/`，
//! 來源是 https://github.com/Tsun-u/tsunu-pet 的 `mod/`），啟動時用 `--plugin-dir` 帶進去。
use serde::{Deserialize, Serialize};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager};

const BUNDLED_MOD: &str = "tsunu-avatar";

pub struct ModState {
    state_url: Option<String>,
}

#[derive(Deserialize)]
struct Report {
    state: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModLaunchInfo {
    state_url: Option<String>,
    plugin_dir: Option<String>,
}

/// mod 的狀態名稱對到前端立繪；session 結束就回到待機。
fn avatar_state(mod_state: &str) -> Option<&'static str> {
    match mod_state {
        "idle" | "ended" => Some("idle"),
        "thinking" => Some("thinking"),
        "working" => Some("working"),
        "asking" => Some("asking"),
        "error" => Some("error"),
        "complete" => Some("complete"),
        _ => None,
    }
}

/// 開收件埠並在背景收報告。埠開不起來時回傳 `state_url: None`，前端就不帶 mod 啟動。
pub fn start(app: &AppHandle) -> ModState {
    let server = match tiny_http::Server::http("127.0.0.1:0") {
        Ok(server) => server,
        Err(e) => {
            eprintln!("⚠️ mod 狀態收件埠開不起來：{e}");
            return ModState { state_url: None };
        }
    };
    let state_url = server
        .server_addr()
        .to_ip()
        .map(|addr| format!("http://{addr}/state"));
    let app = app.clone();
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            let state = request
                .as_reader()
                .read_to_string(&mut body)
                .ok()
                .and_then(|_| serde_json::from_str::<Report>(&body).ok())
                .and_then(|report| avatar_state(&report.state));
            let status = match state {
                Some(state) => {
                    let _ = app.emit("avatar-state", state);
                    204
                }
                None => 400,
            };
            let _ = request.respond(tiny_http::Response::empty(status));
        }
    });
    ModState { state_url }
}

/// Windows 的 resource 路徑可能帶 `\\?\` 前綴，claude 不認，拿掉。
fn plain_path(path: std::path::PathBuf) -> String {
    let text = path.to_string_lossy().into_owned();
    text.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(text)
}

#[tauri::command]
pub fn mod_launch_info(app: AppHandle, mod_state: tauri::State<ModState>) -> ModLaunchInfo {
    let plugin_dir = app
        .path()
        .resolve(BUNDLED_MOD, BaseDirectory::Resource)
        .ok()
        .filter(|dir| dir.join(".claude-plugin").join("plugin.json").is_file())
        .map(plain_path);
    ModLaunchInfo { state_url: mod_state.state_url.clone(), plugin_dir }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mod_states_to_avatar() {
        assert_eq!(avatar_state("asking"), Some("asking"));
        assert_eq!(avatar_state("ended"), Some("idle"));
        assert_eq!(avatar_state("unknown"), None);
    }
}
