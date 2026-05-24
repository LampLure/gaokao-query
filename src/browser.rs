use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, OwnedSemaphorePermit};
use chromiumoxide::{
    browser::{Browser, BrowserConfig, HeadlessMode},
    Page,
};
use futures_util::StreamExt;

use crate::data::QueryResult;
use crate::data::QueryStatus;
use crate::ocr;

static INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct BrowserClient {
    _browser: Arc<Browser>,
    page: Arc<Mutex<Page>>,
    log: Option<Arc<Mutex<Vec<String>>>>,
    perf: Option<Arc<Mutex<Vec<crate::data::PerfEvent>>>>,
    step_delay_ms: u64,
    captcha_retries: u32,
    captcha_wait_ms: u64,
    target_url: String,
    instance_id: u64,
    start: std::time::Instant,
    turbo: bool,
}

impl BrowserClient {
    /// Poll until JS expression returns true, or timeout.
    /// turbo mode: 50ms interval, normal: 200ms.
    async fn poll_true(&self, page: &Page, js: &str, max_ms: u64) -> bool {
        let interval = if self.turbo { 50u64 } else { 200u64 };
        let attempts = (max_ms / interval).max(5);
        for _ in 0..attempts {
            let ok: bool = page.evaluate_expression(js).await
                .map(|r| r.into_value().unwrap_or(false))
                .unwrap_or(false);
            if ok { return true; }
            self.sleep_critical(interval).await;
        }
        false
    }

    pub fn set_perf(&mut self, perf: Option<Arc<Mutex<Vec<crate::data::PerfEvent>>>>) {
        self.perf = perf;
        self.start = std::time::Instant::now();
        self.perf_event("浏览器获取完成");
    }

    pub fn set_turbo(&mut self, turbo: bool) {
        self.turbo = turbo;
    }

    fn perf_event(&self, label: &'static str) {
        let elapsed = self.start.elapsed().as_millis() as u64;
        if let Some(p) = &self.perf {
            if let Ok(mut perf) = p.try_lock() {
                perf.push(crate::data::PerfEvent { label, elapsed_ms: elapsed });
            }
        }
    }

    async fn sleep_step(&self, factor: f64) {
        let ms = if self.turbo {
            30u64
        } else {
            (self.step_delay_ms as f64 * factor) as u64
        };
        tokio::time::sleep(std::time::Duration::from_millis(ms.max(30))).await;
    }

    async fn sleep_critical(&self, ms: u64) {
        let actual = if self.turbo { ms.min(1500) } else { ms };
        tokio::time::sleep(std::time::Duration::from_millis(actual)).await;
    }

    pub async fn new_with_log(
        _headed: bool,
        log: Option<Arc<Mutex<Vec<String>>>>,
        step_delay_ms: u64,
        captcha_retries: u32,
        captcha_wait_ms: u64,
        hide_browser: bool,
        target_url: &str,
    ) -> Result<Self, String> {
        Self::new_with_perf(_headed, log, None, step_delay_ms, captcha_retries, captcha_wait_ms, hide_browser, target_url, false).await
    }

    pub async fn new_with_perf(
        _headed: bool,
        log: Option<Arc<Mutex<Vec<String>>>>,
        perf: Option<Arc<Mutex<Vec<crate::data::PerfEvent>>>>,
        step_delay_ms: u64,
        captcha_retries: u32,
        captcha_wait_ms: u64,
        hide_browser: bool,
        target_url: &str,
        turbo: bool,
    ) -> Result<Self, String> {
        let chrome_path = find_chrome()
            .ok_or_else(|| "未找到Chrome/Chromium浏览器。请安装Chrome后重试。".to_string())?;

        let chrome_name = chrome_path.file_stem()
            .and_then(|s| s.to_str()).unwrap_or("chrome").to_string();

        let instance_id = INSTANCE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let user_data_dir = format!("/tmp/chromiumoxide-runner-{}", instance_id);
        let _ = std::fs::create_dir_all(format!("/tmp/gaokao-captcha-{}", instance_id));

        let mut builder = BrowserConfig::builder()
            .chrome_executable(chrome_path)
            .headless_mode(HeadlessMode::False)
            .user_data_dir(&user_data_dir)
            .arg("--disable-blink-features=AutomationControlled");

        if hide_browser {
            builder = builder.arg("--window-size=1280,720");
            builder = builder.arg("--window-position=-32000,-32000");
            builder = builder.arg("--start-minimized");
        }

        let config = builder.build()
            .map_err(|e| format!("浏览器配置失败: {}", e))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| format!("浏览器启动失败: {}", e))?;

        let browser = Arc::new(browser);

        let browser_clone = browser.clone();
        tokio::spawn(async move { loop { let _ = handler.next().await; } });

        // On Linux, try to hide/minimize Chrome window via external tools
        if hide_browser {
            let cn = chrome_name.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                for args in &[
                    vec!["search", "--name", &cn, "windowminimize"],
                    vec!["search", "--class", &cn, "windowminimize"],
                    vec!["search", "--name", "chromium", "windowminimize"],
                    vec!["search", "--class", "chromium-browser", "windowminimize"],
                ] {
                    let _ = std::process::Command::new("xdotool").args(args).output();
                }
                let _ = std::process::Command::new("wmctrl")
                    .args(["-a", &cn, "-b", "add,hidden"]).output();
            });
        }

        let url = target_url.to_string();
        let page = browser_clone
            .new_page(&url)
            .await
            .map_err(|e| format!("打开页面失败: {}", e))?;

        // Poll for form element (15s timeout) instead of blind 3s sleep
        let interval = if turbo { 50u64 } else { 200u64 };
        let attempts = 15000 / interval;
        for _ in 0..attempts {
            let ready: bool = page.evaluate_expression(
                r#"(function() {
                    const el = document.getElementById('zkzh');
                    return el !== null;
                })()"#
            ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);
            if ready { break; }
            tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
        }

        let now = std::time::Instant::now();
        Ok(Self {
            _browser: browser,
            page: Arc::new(Mutex::new(page)),
            log,
            perf,
            step_delay_ms,
            captcha_retries,
            captcha_wait_ms,
            target_url: url,
            instance_id,
            start: now,
            turbo,
        })
    }

    pub async fn go_home(&self) -> Result<(), String> {
        let page = self.page.lock().await;
        page.goto(&self.target_url)
            .await
            .map_err(|e| format!("导航回首页失败: {}", e))?;

        // Poll for form element to confirm page loaded (10s timeout)
        self.poll_true(&page,
            r#"(function() {
                const el = document.getElementById('zkzh');
                return el !== null;
            })()"#, 10000).await;

        self.perf_event("导航回首页");
        Ok(())
    }

    pub async fn query_single(
        &self,
        baominghao: &str,
        shenfenzheng: &str,
    ) -> Result<QueryResult, String> {
        self.perf_event("开始查询");
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

        self.sleep_step(0.5).await;
        self.perf_event("填写信息完成");

        let _ = page.evaluate_expression(
            r#"(function() {
                const btn = document.querySelector('button[type="submit"]');
                if (btn) { btn.click(); return 'clicked'; }
                return 'no_button';
            })()"#
        ).await;

        // Poll for captcha modal or result (15s timeout)
        self.perf_event("等待验证码");
        let captcha_visible = self.poll_true(&page,
            r#"(function() {
                const m = document.getElementById('captchaModal');
                return m ? !m.classList.contains('hidden') : false;
            })()"#, 15000).await;
        if captcha_visible {
            self.perf_event("验证码弹窗出现");
            if let Err(e) = self.solve_captcha_modal(&page).await {
                self.perf_event("总耗时");
                return Err(format!("验证码处理失败: {}", e));
            }
            self.sleep_critical(200).await;
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

            self.perf_event("总耗时");
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
            self.perf_event("总耗时");
            return Err("未找到查询结果".to_string());
        }
        self.perf_event("获取查询结果");

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

        self.perf_event("总耗时");

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

    fn captcha_dir(&self) -> String {
        format!("/tmp/gaokao-captcha-{}", self.instance_id)
    }

    fn captcha_path(&self) -> String {
        format!("{}/captcha.png", self.captcha_dir())
    }

    async fn solve_captcha_modal(&self, page: &Page) -> Result<(), String> {
        let max_retries = self.captcha_retries;
        let temp_path = self.captcha_path();

        for attempt in 1..=max_retries {
            if attempt > 1 {
                // Refresh captcha button click to get a new image
                self.log_msg("刷新验证码...");
                let _ = page.evaluate_expression(
                    r#"(function() {
                        const btn = document.getElementById('refreshCaptcha');
                        if (btn) { btn.click(); return 'ok'; }
                        return 'no_btn';
                    })()"#
                ).await;
                // Poll for captcha image element after refresh (up to 3s)
                let interval = if self.turbo { 100u64 } else { 300u64 };
                for _ in 0..(3000 / interval).max(5) {
                    let has_img: bool = page.evaluate_expression(
                        r#"!!document.getElementById('captchaImage')"#
                    ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);
                    if has_img { break; }
                    self.sleep_critical(interval).await;
                }
            }
            self.log_msg(&format!("验证码第 {}/{} 次尝试", attempt, max_retries));

            // Poll for captcha image (15s timeout), fetch via JS immediately when ready
            let img_src: String = {
                let interval = if self.turbo { 50u64 } else { 200u64 };
                let attempts = (15000 / interval).max(10);
                let mut last = String::new();
                for _ in 0..attempts {
                    let src: String = page.evaluate_expression(
                        r#"(function() {
                            const img = document.getElementById('captchaImage');
                            if (!img) return '';
                            const loaded = img.complete && img.naturalWidth > 0;
                            const s = img.getAttribute('src') || '';
                            if (s && !s.includes('svg+xml') && s.length > 100) return s;
                            if (loaded && s && s.startsWith('http')) {
                                try {
                                    const xhr = new XMLHttpRequest();
                                    xhr.open('GET', s, false);
                                    xhr.responseType = 'blob';
                                    xhr.send();
                                    const reader = new FileReaderSync();
                                    return reader.readAsDataURL(xhr.response);
                                } catch(e) {}
                            }
                            if (loaded) {
                                try {
                                    const c = document.createElement('canvas');
                                    c.width = img.naturalWidth;
                                    c.height = img.naturalHeight;
                                    const ctx = c.getContext('2d');
                                    ctx.drawImage(img, 0, 0);
                                    return c.toDataURL('image/png');
                                } catch(e) {}
                            }
                            return '';
                        })()"#
                    ).await.map_err(|e| format!("获取验证码失败: {}", e))?
                        .into_value().unwrap_or_default();
                    if !src.is_empty() && src.len() > 100 {
                        last = src;
                        break;
                    }
                    self.sleep_critical(interval).await;
                }
                if last.is_empty() {
                    return Err("验证码图片加载超时".to_string());
                }
                last
            };
            self.perf_event("验证码图片加载完成");

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

            std::fs::write(&temp_path, &img_bytes)
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

            self.perf_event("获取验证码图片");

            // Solve captcha via OCR
            self.perf_event("OCR开始");
            let ocr_result = match ocr::solve_captcha(&temp_path, &expected_chars, cw, ch, self.instance_id).await {
                Ok(r) => {
                    self.perf_event("OCR完成");
                    r
                }
                Err(e) => {
                    self.log_msg(&format!("OCR失败: {}, 准备重试", e));
                    // Save failed captcha for debugging
                    let debug_path = format!("{}/fail_{}.png", self.captcha_dir(), attempt);
                    let _ = std::fs::copy(&temp_path, &debug_path);
                    self.log_msg(&format!("已保存失败验证码到: {}", debug_path));
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
            self.perf_event("验证码点击开始");
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
                self.sleep_step(0.3).await;
            }

            // Poll for verification result (200ms interval, server usually responds in 0.5-2s)
            self.perf_event("验证码点击完成");
            // Wait at least 2s total (matching original 2s blind sleep)
            // but with polling so early success is detected faster
            let interval = if self.turbo { 100u64 } else { 200u64 };
            let max_polls = (2000 / interval).max(5);
            let mut still_visible = true;
            for _ in 0..max_polls {
                let r: bool = page.evaluate_expression(
                    r#"(function() {
                        const m = document.getElementById('captchaModal');
                        return m ? !m.classList.contains('hidden') : false;
                    })()"#
                ).await.map(|v| v.into_value().unwrap_or(true)).unwrap_or(true);
                if !r {
                    still_visible = false;
                    break;
                }
                self.sleep_critical(interval).await;
            }

            if !still_visible {
                // Captcha passed
                self.log_msg("验证码通过");
                self.perf_event("验证码验证通过");
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

            self.sleep_critical(500).await;
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

pub struct BrowserPool {
    clients: Mutex<VecDeque<BrowserClient>>,
    semaphore: Arc<Semaphore>,
}

impl BrowserPool {
    pub async fn new(
        count: usize,
        hide_browser: bool,
        step_delay_ms: u64,
        captcha_retries: u32,
        captcha_wait_ms: u64,
        target_url: &str,
        logs: Option<Arc<Mutex<Vec<String>>>>,
        turbo: bool,
    ) -> Result<Arc<Self>, String> {
        let launch_delay = if step_delay_ms > 0 && !turbo { step_delay_ms } else { 100 };
        let mut handles = Vec::with_capacity(count);
        for i in 0..count {
            let url = target_url.to_string();
            let l = logs.clone();
            handles.push(tokio::spawn(async move {
                BrowserClient::new_with_perf(false, l, None, step_delay_ms, captcha_retries, captcha_wait_ms, hide_browser, &url, turbo).await
            }));
            if i + 1 < count {
                tokio::time::sleep(std::time::Duration::from_millis(launch_delay)).await;
            }
        }
        let mut clients = VecDeque::with_capacity(count);
        for (i, h) in handles.into_iter().enumerate() {
            let client = h.await.map_err(|_| "浏览器启动任务异常终止".to_string())?
                .map_err(|e| format!("第{}个浏览器启动失败: {}", i + 1, e))?;
            clients.push_back(client);
        }
        Ok(Arc::new(Self {
            clients: Mutex::new(clients),
            semaphore: Arc::new(Semaphore::new(count)),
        }))
    }

    pub async fn acquire(self: &Arc<Self>) -> (OwnedSemaphorePermit, BrowserClient) {
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let mut clients = self.clients.lock().await;
        let client = clients.pop_front().unwrap();
        (permit, client)
    }

    pub fn release(self: &Arc<Self>, permit: OwnedSemaphorePermit, client: BrowserClient) {
        let this = self.clone();
        tokio::spawn(async move {
            let _ = client.go_home().await;
            this.clients.lock().await.push_back(client);
            drop(permit);
        });
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
