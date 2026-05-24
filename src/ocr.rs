use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

pub struct ClickPoint {
    pub x: f64,
    pub y: f64,
}

pub struct OcrResult {
    pub points: Vec<ClickPoint>,
    pub debug_info: String,
}

fn python_path() -> PathBuf {
    let venv = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".venv").join("bin").join("python3");
    if venv.exists() { venv } else { PathBuf::from("python3") }
}

pub async fn solve_captcha(
    image_path: &str,
    expected_chars: &[String],
    _container_width: f64,
    _container_height: f64,
    instance_id: u64,
) -> Result<OcrResult, String> {
    let expected = expected_chars.join(" ");

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(python_path())
            .arg("ocr_helper.py")
            .arg(image_path)
            .arg(&expected)
            .arg(instance_id.to_string())
            .output(),
    )
    .await
    .map_err(|_| format!("OCR进程超时（30秒）"))?
    .map_err(|e| format!("无法启动OCR进程: {}", e))?;

    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    let stdout = String::from_utf8_lossy(&result.stdout).to_string();

    if !result.status.success() {
        return Err(format!("OCR失败: {}", stderr));
    }

    let points: Vec<ClickPoint> = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.trim().split(',').collect();
            if parts.len() == 2 {
                let x: f64 = parts[0].parse().ok()?;
                let y: f64 = parts[1].parse().ok()?;
                Some(ClickPoint { x, y })
            } else {
                None
            }
        })
        .collect();

    if points.len() != 3 {
        return Err(format!(
            "OCR返回了{}个点, 预期3个\nstderr: {}",
            points.len(),
            stderr
        ));
    }

    Ok(OcrResult {
        points,
        debug_info: stderr,
    })
}
