use std::process::Command;

/// Click position as fraction (0.0-1.0) of the container dimensions
pub struct ClickPoint {
    pub x: f64,
    pub y: f64,
}

pub fn solve_captcha(
    image_path: &str,
    expected_chars: &[String],
    _container_width: f64,
    _container_height: f64,
) -> Result<Vec<ClickPoint>, String> {
    let expected = expected_chars.join(" ");
    let output = Command::new("python3")
        .arg("/tmp/gaokao_env/ocr_helper.py")
        .arg(image_path)
        .arg(&expected)
        .output()
        .map_err(|e| format!("无法启动OCR进程: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("OCR失败: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
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
        return Err(format!("OCR返回了{}个点, 预期3个", points.len()));
    }

    Ok(points)
}
