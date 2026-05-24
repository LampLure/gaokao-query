use chromiumoxide::{
    browser::{Browser, BrowserConfig, HeadlessMode},
    Page,
};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::data::QueryResult;
use crate::data::QueryStatus;
use crate::ocr;

const TARGET_URL: &str = "https://cx.hbea.edu.cn/gkkd/2026/eb3f6190-590c-4f79-9b88-81a1d0aa0a2b";

pub struct BrowserClient {
    _browser: Arc<Browser>,
    page: Arc<Mutex<Page>>,
    log: Option<Arc<Mutex<Vec<String>>>>,
}

impl BrowserClient {
    pub async fn new(headed: bool) -> Result<Self, String> {
        Self::new_with_log(headed, None).await
    }

    pub async fn new_with_log(
        headed: bool,
        log: Option<Arc<Mutex<Vec<String>>>>,
    ) -> Result<Self, String> {
        let chrome_path = find_chrome()
            .ok_or_else(|| "未找到Chrome/Chromium浏览器。请安装Chrome后重试。".to_string())?;

        let headless = if headed {
            HeadlessMode::False
        } else {
            HeadlessMode::New
        };

        let config = BrowserConfig::builder()
            .chrome_executable(chrome_path)
            .headless_mode(headless)
            .build()
            .map_err(|e| format!("浏览器配置失败: {}", e))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| format!("浏览器启动失败: {}", e))?;

        let browser = Arc::new(browser);

        let browser_clone = browser.clone();
        tokio::spawn(async move { loop { let _ = handler.next().await; } });

        let page = browser_clone
            .new_page(TARGET_URL)
            .await
            .map_err(|e| format!("打开页面失败: {}", e))?;

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        Ok(Self {
            _browser: browser,
            page: Arc::new(Mutex::new(page)),
            log,
        })
    }

    pub async fn query_single(
        &self,
        baominghao: &str,
        shenfenzheng: &str,
    ) -> Result<QueryResult, String> {
        let page = self.page.lock().await;

        fn js_fill(id: &str, val: &str) -> String {
            format!(
                "(function(){{const el=document.getElementById('{}');if(!el)return'no_{}';const setter=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value').set;setter.call(el,'{}');el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return'ok';}})()",
                id, id, val
            )
        }

        let _: String = page.evaluate_expression(js_fill("zkzh", baominghao))
            .await.map_err(|e| format!("填报名号失败: {}", e))?
            .into_value().map_err(|_| "填报名号返回值解析失败".to_string())?;

        let _: String = page.evaluate_expression(js_fill("sfzh", shenfenzheng))
            .await.map_err(|e| format!("填身份证号失败: {}", e))?
            .into_value().map_err(|_| "填身份证返回值解析失败".to_string())?;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let _ = page.evaluate_expression(
            r#"(function() {
                const btn = document.querySelector('button[type="submit"]');
                if (btn) { btn.click(); return 'clicked'; }
                return 'no_button';
            })()"#
        ).await;

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let captcha_visible: bool = page.evaluate_expression(
            r#"(function() {
                const m = document.getElementById('captchaModal');
                return m ? !m.classList.contains('hidden') : false;
            })()"#
        ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);

        if captcha_visible {
            if let Err(e) = self.solve_captcha_modal(&page).await {
                return Err(format!("验证码处理失败: {}", e));
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        let has_error: bool = page.evaluate_expression(
            r#"(function() {
                const m = document.getElementById('alertModal');
                return m ? !m.classList.contains('hidden') : false;
            })()"#
        ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);

        if has_error {
            let err_msg: String = page.evaluate_expression(
                r#"(function() {
                    const el = document.getElementById('alertMessage');
                    return el ? el.textContent || '' : '';
                })()"#
            ).await.map(|r| r.into_value().unwrap_or_default()).unwrap_or_default();

            let _ = page.evaluate_expression(
                r#"(function() {
                    const btn = document.getElementById('alertOkButton');
                    if (btn) btn.click();
                })()"#
            ).await;

            return Err(err_msg);
        }

        let name: String = page.evaluate_expression(
            r#"(function() {
                const el = document.querySelector('[data-value="xm"]');
                return el ? el.textContent.trim() : '';
            })()"#
        ).await.map_err(|e| format!("获取结果失败: {}", e))?
            .into_value().unwrap_or_default();

        if name.is_empty() {
            return Err("未找到查询结果".to_string());
        }

        let bmh: String = page.evaluate_expression(
            r#"(function() {
                const el = document.querySelector('[data-value="ksh"]');
                return el ? el.textContent.trim() : '';
            })()"#
        ).await.map(|r| r.into_value().unwrap_or_default()).unwrap_or_default();

        let kemu: String = page.evaluate_expression(
            r#"(function() {
                const el = document.querySelector('[data-value="kmmc"]');
                return el ? el.textContent.trim() : '';
            })()"#
        ).await.map(|r| r.into_value().unwrap_or_default()).unwrap_or_default();

        let kd: String = page.evaluate_expression(
            r#"(function() {
                const el = document.querySelector('[data-value="kdmc"]');
                return el ? el.textContent.trim() : '';
            })()"#
        ).await.map(|r| r.into_value().unwrap_or_default()).unwrap_or_default();

        Ok(QueryResult {
            name,
            baominghao: bmh,
            shenfenzheng: shenfenzheng.to_string(),
            kemumingcheng: kemu,
            kaodianmingcheng: kd,
            status: QueryStatus::Success,
            error: String::new(),
        })
    }

    async fn solve_captcha_modal(&self, page: &Page) -> Result<(), String> {
        let max_retries = 3;
        let temp_path = "/tmp/gaokao_captcha.png";

        for attempt in 1..=max_retries {
            if attempt > 1 {
                // Wait for new captcha to load
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            self.log_msg(&format!("验证码第 {}/{} 次尝试", attempt, max_retries));

            // Get captcha image (wait for it to actually load)
            let img_src: String = {
                let mut last_src = String::new();
                for _ in 0..20 {
                    let src: String = page.evaluate_expression(
                        r#"(function() {
                            const img = document.getElementById('captchaImage');
                            if (!img) return '';
                            const s = img.getAttribute('src') || '';
                            // Only return non-placeholder, non-empty images
                            if (s && !s.includes('svg+xml') && s.length > 100) return s;
                            return '';
                        })()"#
                    ).await.map_err(|e| format!("获取验证码失败: {}", e))?
                        .into_value().unwrap_or_default();

                    if !src.is_empty() {
                        last_src = src;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }

                if last_src.is_empty() {
                    return Err("验证码图片加载超时".to_string());
                }
                last_src
            };

            // Get expected chars
            let chars_text: String = page.evaluate_expression(
                r#"(function() {
                    const spans = document.querySelectorAll('#captchaChars span');
                    return Array.from(spans).map(s => s.textContent.trim()).join(' ');
                })()"#
            ).await.map_err(|e| format!("获取验证码文字失败: {}", e))?
                .into_value().unwrap_or_default();

            let expected_chars: Vec<String> =
                chars_text.split_whitespace().map(|s| s.to_string()).collect();
            if expected_chars.len() != 3 {
                return Err(format!("验证码字符数错误: {}", expected_chars.len()));
            }

            // Save image
            let b64_data = img_src
                .strip_prefix("data:image/png;base64,")
                .or_else(|| img_src.strip_prefix("data:image/jpeg;base64,"))
                .unwrap_or(&img_src);

            use base64::Engine;
            let img_bytes = base64::engine::general_purpose::STANDARD
                .decode(b64_data).map_err(|e| format!("base64解码失败: {}", e))?;

            std::fs::write(temp_path, &img_bytes)
                .map_err(|e| format!("保存验证码图片失败: {}", e))?;

            // Get container dimensions
            let dims_json: String = page.evaluate_expression(
                r#"(function() {
                    const el = document.getElementById('captchaContainer');
                    if (!el) return JSON.stringify({w:300, h:150});
                    const rect = el.getBoundingClientRect();
                    return JSON.stringify({w: rect.width, h: rect.height});
                })()"#
            ).await.map(|r| r.into_value().unwrap_or_else(|_| r#"{"w":300,"h":150}"#.to_string()))
                .unwrap_or_else(|_| r#"{"w":300,"h":150}"#.to_string());

            let dims: serde_json::Value =
                serde_json::from_str(&dims_json).unwrap_or(serde_json::json!({"w":300,"h":150}));
            let cw = dims["w"].as_f64().unwrap_or(300.0);
            let ch = dims["h"].as_f64().unwrap_or(150.0);

            // Solve captcha via OCR
            let ocr_result = match ocr::solve_captcha(temp_path, &expected_chars, cw, ch) {
                Ok(r) => r,
                Err(e) => {
                    self.log_msg(&format!("OCR失败: {}, 准备重试", e));
                    continue;
                }
            };

            // Log OCR debug
            if let Some(log) = &self.log {
                if let Ok(mut l) = log.try_lock() {
                    for line in ocr_result.debug_info.lines() {
                        l.push(format!("[OCR] {}", line));
                    }
                }
            }

            // Click points
            for point in &ocr_result.points {
                let click_js = format!(
                    r#"(function() {{
                        const container = document.getElementById('captchaContainer');
                        if (!container) return 'no_container';
                        const rect = container.getBoundingClientRect();
                        const x = rect.left + {};
                        const y = rect.top + {};
                        container.dispatchEvent(new MouseEvent('click', {{
                            clientX: x, clientY: y, bubbles: true, cancelable: true
                        }}));
                        return 'clicked';
                    }})()"#,
                    point.x * cw, point.y * ch
                );
                let _ = page.evaluate_expression(&click_js).await;
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }

            // Wait for verification result
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // Check if captcha modal is still visible (verification failed)
            let still_visible: bool = page.evaluate_expression(
                r#"(function() {
                    const m = document.getElementById('captchaModal');
                    return m ? !m.classList.contains('hidden') : false;
                })()"#
            ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);

            if !still_visible {
                // Captcha passed
                self.log_msg("验证码通过");
                return Ok(());
            }

            // Captcha failed - dismiss error alert if present
            self.log_msg("验证码验证失败，尝试关闭弹窗并重试");

            let _ = page.evaluate_expression(
                r#"(function() {
                    // Try clicking the alert OK button first
                    const okBtn = document.getElementById('alertOkButton');
                    if (okBtn) { okBtn.click(); return 'alert_dismissed'; }
                    // If no alert, maybe the captcha modal has a close button
                    const closeBtn = document.querySelector('.close-modal');
                    if (closeBtn) { closeBtn.click(); return 'closed'; }
                    // Try refresh captcha
                    const refreshBtn = document.getElementById('refreshCaptcha');
                    if (refreshBtn) { refreshBtn.click(); return 'refreshed'; }
                    return 'nothing';
                })()"#
            ).await;

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(format!("验证码连续失败 {} 次，已放弃", max_retries))
    }

    fn log_msg(&self, msg: &str) {
        if let Some(log) = &self.log {
            if let Ok(mut l) = log.try_lock() {
                l.push(format!("[CAPTCHA] {}", msg));
            }
        }
    }
}

fn find_chrome() -> Option<std::path::PathBuf> {
    for p in &[
        "/usr/bin/chromium-browser",
        "/usr/bin/chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/snap/bin/chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
        "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    ] {
        let path = std::path::Path::new(p);
        if path.exists() {
            return Some(path.to_path_buf());
        }
    }
    None
}
