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

/// OCR 服务端口，每个浏览器实例对应一个端口（instance_id + 19999）
fn ocr_port(instance_id: u64) -> u16 {
    19999 + (instance_id % 10) as u16
}

/// ────────────────────────────────────────────────────────
/// 优先使用常驻 OCR HTTP 服务（模型只加载一次，0.3-0.8s/次）
/// 如果服务不可用，自动降级为子进程模式（兼容旧流程）
/// ────────────────────────────────────────────────────────
pub async fn solve_captcha(
    image_path: &str,
    expected_chars: &[String],
    _container_width: f64,
    _container_height: f64,
    instance_id: u64,
) -> Result<OcrResult, String> {
    // 读取图片并 base64 编码
    let img_bytes = std::fs::read(image_path)
        .map_err(|e| format!("读取验证码图片失败: {}", e))?;
    use base64::Engine;
    let img_b64 = base64::engine::general_purpose::STANDARD.encode(&img_bytes);

    let port = ocr_port(instance_id);

    // 优先尝试 HTTP 常驻服务
    match try_http_ocr(&img_b64, expected_chars, port).await {
        Ok(result) => return Ok(result),
        Err(e) => {
            // HTTP 服务不可用，降级到子进程模式
            eprintln!("[OCR] HTTP服务不可用(port {}): {}，降级到子进程模式", port, e);
        }
    }

    // 子进程兜底（原有逻辑）
    solve_captcha_subprocess(image_path, expected_chars, instance_id).await
}

/// HTTP 常驻 OCR 服务调用
async fn try_http_ocr(
    img_b64: &str,
    expected_chars: &[String],
    port: u16,
) -> Result<OcrResult, String> {
    let body = serde_json::json!({
        "image": img_b64,
        "expected_chars": expected_chars,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP客户端创建失败: {}", e))?;

    let result = tokio::time::timeout(
        Duration::from_secs(15),
        client.post(format!("http://127.0.0.1:{}/", port))
            .json(&body)
            .send(),
    ).await
        .map_err(|_| "OCR HTTP服务请求超时（15秒）".to_string())?
        .map_err(|e| format!("OCR HTTP服务请求失败: {}", e))?;

    if !result.status().is_success() {
        return Err(format!("OCR HTTP服务返回错误状态: {}", result.status()));
    }

    let resp: serde_json::Value = result.json().await
        .map_err(|e| format!("OCR HTTP服务响应解析失败: {}", e))?;

    let points_json = resp.get("points")
        .ok_or_else(|| "OCR服务返回缺少 points 字段".to_string())?
        .as_array()
        .ok_or_else(|| "points 不是数组".to_string())?;

    let mut points = Vec::new();
    for p in points_json {
        // 支持两种格式：[x, y] 数组 或 {"x":..., "y":...} 对象
        if let Some(arr) = p.as_array() {
            let x = arr.get(0).and_then(|v| v.as_f64()).unwrap_or(0.5);
            let y = arr.get(1).and_then(|v| v.as_f64()).unwrap_or(0.5);
            points.push(ClickPoint { x, y });
        } else {
            let x = p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.5);
            let y = p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.5);
            points.push(ClickPoint { x, y });
        }
    }

    if points.len() != expected_chars.len() {
        return Err(format!(
            "OCR 返回点数({})与期望字符数({})不匹配",
            points.len(), expected_chars.len()
        ));
    }

    let strategy = resp.get("strategy").and_then(|v| v.as_str()).unwrap_or("http");
    let debug = resp.get("debug").and_then(|v| v.as_str()).unwrap_or("OK");

    Ok(OcrResult {
        points,
        debug_info: format!("[OCR HTTP] strategy={}, {}", strategy, debug),
    })
}

/// 子进程模式（原有逻辑，作为降级方案保留）
async fn solve_captcha_subprocess(
    image_path: &str,
    expected_chars: &[String],
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
    .map_err(|e| format!("无法启动 OCR 进程：{}", e))?;

    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    let stdout = String::from_utf8_lossy(&result.stdout).to_string();

    if !result.status.success() {
        return Err(format!("OCR 失败：{}", stderr));
    }

    // --- 支持汉字语义绑定的 OCR 识别块结构 ---
    struct DetBlock {
        x: f64,
        y: f64,
        text: String,
    }

    let mut det_blocks: Vec<DetBlock> = Vec::new();
    let mut fallback_points: Vec<ClickPoint> = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.trim().split(',').collect();
        if parts.len() >= 2 {
            let x: f64 = match parts[0].parse() { Ok(v) => v, Err(_) => continue };
            let y: f64 = match parts[1].parse() { Ok(v) => v, Err(_) => continue };
            
            let text = if parts.len() >= 3 { parts[2].trim().to_string() } else { String::new() };
            
            det_blocks.push(DetBlock { x, y, text: text.clone() });
            fallback_points.push(ClickPoint { x, y });
        }
    }

    let mut final_click_points = Vec::new();
    let mut debug_info = stderr.clone();
    debug_info.push_str("\n[OCR 语义重排流开始]\n");

    for (step, target_char) in expected_chars.iter().enumerate() {
        let mut found_match = false;
        
        for block in &det_blocks {
            if !block.text.is_empty() && block.text == target_char.trim() {
                final_click_points.push(ClickPoint { x: block.x, y: block.y });
                debug_info.push_str(&format!(
                    "   🎯 [语义精准锁定] 目标字 [{}]: 匹配到坐标 ({:.4}, {:.4})，赋予第 {} 次点击\n",
                    target_char, block.x, block.y, step + 1
                ));
                found_match = true;
                break;
            }
        }

        if !found_match {
            if let Some(fb_point) = fallback_points.get(step) {
                final_click_points.push(ClickPoint { x: fb_point.x, y: fb_point.y });
                debug_info.push_str(&format!(
                    "   ⚠️ [语义错字兜底] 目标字 [{}] 未能直接识别，降级使用物理第 {} 个框坐标 -> ({:.4}, {:.4})\n",
                    target_char, step + 1, fb_point.x, fb_point.y
                ));
            }
        }
    }

    if final_click_points.len() != expected_chars.len() {
        return Err(format!(
            "OCR 编排点击流失败，期望 {} 个点，实际完成 {} 个点\nstdout: {}",
            expected_chars.len(), final_click_points.len(), stdout
        ));
    }

    Ok(OcrResult {
        points: final_click_points,
        debug_info,
    })
}
