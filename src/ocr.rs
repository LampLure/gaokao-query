use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// 已尝试启动的 OCR 服务端口集合（避免重复启动）
static STARTED_PORTS: std::sync::LazyLock<std::sync::Mutex<Vec<u16>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// OCR HTTP 服务健康检查是否通过的缓存（避免每次都探测）
static HEALTH_CACHE: std::sync::LazyLock<std::sync::Mutex<Vec<u16>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// 自动启动 OCR HTTP 服务（如果未运行）
async fn ensure_ocr_server(port: u16) {
    // 检查是否已启动过
    {
        let started = STARTED_PORTS.lock().unwrap();
        if started.contains(&port) {
            return;
        }
    }

    // 先检查服务是否已经在运行
    if check_ocr_health(port).await {
        let mut started = STARTED_PORTS.lock().unwrap();
        if !started.contains(&port) {
            started.push(port);
        }
        let mut cache = HEALTH_CACHE.lock().unwrap();
        if !cache.contains(&port) {
            cache.push(port);
        }
        return;
    }

    // 服务未运行，尝试启动
    eprintln!("[OCR] HTTP服务未运行(port {})，正在自动启动...", port);
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ocr_server.py");
    if !script.exists() {
        eprintln!("[OCR] ocr_server.py 不存在，跳过自动启动");
        return;
    }

    let py = python_path();
    let child = Command::new(&py)
        .arg(&script)
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match child {
        Ok(_) => {
            eprintln!("[OCR] 已启动 ocr_server.py (port {})，等待服务就绪...", port);
            // 等待服务启动完成（最多30秒，模型加载需要时间）
            for _ in 0..60 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if check_ocr_health(port).await {
                    eprintln!("[OCR] HTTP服务已就绪 (port {})", port);
                    let mut started = STARTED_PORTS.lock().unwrap();
                    if !started.contains(&port) {
                        started.push(port);
                    }
                    let mut cache = HEALTH_CACHE.lock().unwrap();
                    if !cache.contains(&port) {
                        cache.push(port);
                    }
                    return;
                }
            }
            eprintln!("[OCR] HTTP服务启动超时 (port {})，将降级到子进程模式", port);
        }
        Err(e) => {
            eprintln!("[OCR] 启动 ocr_server.py 失败: {}，将降级到子进程模式", e);
        }
    }
}

/// 检查 OCR HTTP 服务是否在运行（轻量健康检查）
async fn check_ocr_health(port: u16) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .connect_timeout(Duration::from_millis(300))
        .build();

    let client = match client {
        Ok(c) => c,
        Err(_) => return false,
    };

    // 发一个极小的请求来检查服务是否存活
    let body = serde_json::json!({
        "image": "",
        "expected_chars": ["test"],
    });

    match client.post(format!("http://127.0.0.1:{}/", port))
        .json(&body)
        .timeout(Duration::from_millis(800))
        .send().await
    {
        Ok(_) => true,
        Err(_) => false,
    }
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
    let port = ocr_port(instance_id);

    // 检查健康缓存，如果已知服务可用则直接用
    let use_http = {
        let cache = HEALTH_CACHE.lock().unwrap();
        cache.contains(&port)
    };

    if !use_http {
        // 尝试启动或确认服务可用
        ensure_ocr_server(port).await;
    }

    // 读取图片并 base64 编码
    let img_bytes = std::fs::read(image_path)
        .map_err(|e| format!("读取验证码图片失败: {}", e))?;
    use base64::Engine;
    let img_b64 = base64::engine::general_purpose::STANDARD.encode(&img_bytes);

    // 尝试 HTTP 常驻服务
    match try_http_ocr(&img_b64, expected_chars, port).await {
        Ok(result) => {
            // 成功，确保端口在健康缓存中
            let mut cache = HEALTH_CACHE.lock().unwrap();
            if !cache.contains(&port) {
                cache.push(port);
            }
            return Ok(result);
        }
        Err(e) => {
            // HTTP 服务不可用，从健康缓存中移除
            {
                let mut cache = HEALTH_CACHE.lock().unwrap();
                cache.retain(|&p| p != port);
            }
            eprintln!("[OCR] HTTP服务不可用(port {}): {}，降级到子进程模式", port, e);
        }
    }

    // 子进程兜底
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
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(1))
        .pool_max_idle_per_host(1)
        .build()
        .map_err(|e| format!("HTTP客户端创建失败: {}", e))?;

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.post(format!("http://127.0.0.1:{}/", port))
            .json(&body)
            .send(),
    ).await
        .map_err(|_| "OCR HTTP服务请求超时（10秒）".to_string())?
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

    // 修复坐标重复bug：跟踪已使用的框索引
    let mut used_box_indices: Vec<usize> = Vec::new();

    for (step, target_char) in expected_chars.iter().enumerate() {
        let mut found_match = false;

        for (block_idx, block) in det_blocks.iter().enumerate() {
            if !block.text.is_empty() && block.text == target_char.trim() && !used_box_indices.contains(&block_idx) {
                final_click_points.push(ClickPoint { x: block.x, y: block.y });
                debug_info.push_str(&format!(
                    "   🎯 [语义精准锁定] 目标字 [{}]: 匹配到坐标 ({:.4}, {:.4})，赋予第 {} 次点击\n",
                    target_char, block.x, block.y, step + 1
                ));
                used_box_indices.push(block_idx);
                found_match = true;
                break;
            }
        }

        if !found_match {
            // 使用未被占用的框坐标（修复坐标重复bug）
            let unused_idx = (0..fallback_points.len())
                .find(|i| !used_box_indices.contains(i))
                .unwrap_or(step.min(fallback_points.len().saturating_sub(1)));

            if unused_idx < fallback_points.len() {
                let fb_point = &fallback_points[unused_idx];
                final_click_points.push(ClickPoint { x: fb_point.x, y: fb_point.y });
                used_box_indices.push(unused_idx);
                debug_info.push_str(&format!(
                    "   ⚠️ [语义错字兜底] 目标字 [{}] 未能直接识别，降级使用未占用框{}坐标 -> ({:.4}, {:.4})\n",
                    target_char, unused_idx + 1, fb_point.x, fb_point.y
                ));
            } else {
                // 无可用框，用中心点
                final_click_points.push(ClickPoint { x: 0.5, y: 0.5 });
                debug_info.push_str(&format!(
                    "   ⚠️ [语义错字兜底] 目标字 [{}] 无可用框，使用中心点\n",
                    target_char
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

    // 检查坐标重复并微调
    let mut seen_coords: Vec<(f64, f64)> = Vec::new();
    for (i, point) in final_click_points.iter_mut().enumerate() {
        if seen_coords.iter().any(|(x, y)| (*x - point.x).abs() < 0.001 && (*y - point.y).abs() < 0.001) {
            // 坐标重复，微调
            point.x += 0.02;
            debug_info.push_str(&format!(
                "   FIX: 第{}个点坐标重复，微调到 ({:.4}, {:.4})\n",
                i + 1, point.x, point.y
            ));
        }
        seen_coords.push((point.x, point.y));
    }

    Ok(OcrResult {
        points: final_click_points,
        debug_info,
    })
}
