use crate::data::AppConfig;
use std::path::PathBuf;

fn config_dir() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("gaokao-query");
    let _ = std::fs::create_dir_all(&p);
    p
}

fn config_path() -> PathBuf {
    let mut p = config_dir();
    p.push("config.json");
    p
}

pub fn load() -> AppConfig {
    let path = config_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(config: &AppConfig) {
    let path = config_path();
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(&path, &json);
    }
}
