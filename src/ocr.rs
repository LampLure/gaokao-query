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
    .map_err(|e| format!("无法启动 OCR 进程：{}", e))?;

    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    let stdout = String::from_utf8_lossy(&result.stdout).to_string();

    if !result.status.success() {
        return Err(format!("OCR 失败：{}", stderr));
    }

    // --- 🟢 重新定义支持汉字语义绑定的 OCR 识别块结构 ---
    struct DetBlock {
        x: f64,
        y: f64,
        text: String,
    }

    // 1. 精准解析 Python 传回的结构（支持行格式：x,y,text）
    let mut det_blocks: Vec<DetBlock> = Vec::new();
    let mut fallback_points: Vec<ClickPoint> = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.trim().split(',').collect();
        if parts.len() >= 2 {
            let x: f64 = match parts[0].parse() { Ok(v) => v, Err(_) => continue };
            let y: f64 = match parts[1].parse() { Ok(v) => v, Err(_) => continue };
            
            // 如果 Python 端传回了第三个参数汉字，做精准配对
            let text = if parts.len() >= 3 { parts[2].trim().to_string() } else { String::new() };
            
            det_blocks.push(DetBlock { x, y, text: text.clone() });
            fallback_points.push(ClickPoint { x, y });
        }
    }

    let mut final_click_points = Vec::new();
    let mut debug_info = stderr.clone();
    debug_info.push_str("\n[OCR 语义重排流开始]\n");

    // 2. 根据提示词要求的标准点击顺序（如：["育", "校", "究"]），重新编排坐标映射
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

        // 3. 兜底策略：如果因为噪点完全没认出来这个字，降级为原生的物理顺序，防止流中断
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
