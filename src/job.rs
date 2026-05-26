use crate::data::PredictionJob;
use std::path::PathBuf;

/// 任务存储目录
fn job_dir() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("gaokao-query");
    p.push("jobs");
    let _ = std::fs::create_dir_all(&p);
    p
}

fn job_path(id: &str) -> PathBuf {
    let mut p = job_dir();
    p.push(format!("{}.json", id));
    p
}

/// 保存任务到磁盘
pub fn save_job(job: &PredictionJob) -> Result<(), String> {
    let path = job_path(&job.id);
    let json = serde_json::to_string_pretty(job)
        .map_err(|e| format!("序列化任务失败: {}", e))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("保存任务失败: {}", e))
}

/// 加载任务
pub fn load_job(id: &str) -> Result<PredictionJob, String> {
    let path = job_path(id);
    let json = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取任务失败: {}", e))?;
    serde_json::from_str(&json)
        .map_err(|e| format!("解析任务失败: {}", e))
}

/// 列出所有任务
pub fn list_jobs() -> Vec<PredictionJob> {
    let dir = job_dir();
    let mut jobs = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(json) = std::fs::read_to_string(&path) {
                    if let Ok(job) = serde_json::from_str::<PredictionJob>(&json) {
                        jobs.push(job);
                    }
                }
            }
        }
    }

    // 按创建时间排序（最新的在前）
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    jobs
}

/// 删除任务
pub fn delete_job(id: &str) -> Result<(), String> {
    let path = job_path(id);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| format!("删除任务失败: {}", e))
    } else {
        Err("任务文件不存在".to_string())
    }
}

/// 计算文件的简易哈希（用于检测文件是否变更）
pub fn file_hash(path: &str) -> String {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    // 简易哈希：文件大小 + 前1KB + 后1KB
    let size = buf.len();
    let head: Vec<u8> = buf.iter().take(1024).copied().collect();
    let tail: Vec<u8> = buf.iter().rev().take(1024).copied().collect();
    format!("{}:{:?}:{:?}", size, head.len(), tail.len())
}
