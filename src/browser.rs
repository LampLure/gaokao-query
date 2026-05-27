use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, OwnedSemaphorePermit};
use chromiumoxide::{
    browser::{Browser, BrowserConfig, HeadlessMode},
    Page,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
};
use futures_util::StreamExt;

use crate::data::{QueryResult, QueryStatus, BrowserStatus, BrowserStep, CaptchaStats};
use crate::ocr;

/// 提交表单后的页面响应状态
enum SubmitResult {
    /// 验证码弹窗出现，需要先解决验证码
    CaptchaNeeded,
    /// 直接进入结果页面（无需验证码或验证码已通过后的结果）
    ResultReady,
    /// 等待超时
    Timeout,
}

static INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct BrowserClient {
    _browser: Arc<Browser>,
    /// CDP handler 任务的 JoinHandle，shutdown 时 abort
    _handler_task: tokio::task::JoinHandle<()>,
    page: Arc<Mutex<Page>>,
    log: Option<Arc<Mutex<Vec<String>>>>,
    perf: Option<Arc<Mutex<Vec<crate::data::PerfEvent>>>>,
    step_delay_ms: u64,
    captcha_retries: u32,
    captcha_wait_ms: u64,
    target_url: String,
    instance_id: u64,
    start: std::time::Instant,
    /// 每个步骤开始时间（用于显示当前步骤耗时）
    step_start: Arc<std::sync::Mutex<std::time::Instant>>,
    turbo: bool,
    status: Option<Arc<Mutex<Vec<BrowserStatus>>>>,
    captcha_stats: Option<Arc<Mutex<CaptchaStats>>>,
}

/// 检测页面当前状态的综合JS表达式
/// 返回值:
///   "form"       - 表单可用（初始状态）
///   "captcha"    - 验证码弹窗可见
///   "result_ok"  - 查询成功结果（resultContainer可见且有姓名数据）
///   "result_err" - 查询无数据结果（resultContainer可见但无姓名，如"未查询到数据"）
///   "alert"      - 错误弹窗可见
///   "unknown"    - 无法识别
const JS_PAGE_STATE: &str = r#"(function() {
    // 1. 检查错误弹窗（最高优先级）
    const alertModal = document.getElementById('alertModal');
    if (alertModal && !alertModal.classList.contains('hidden')) return 'alert';
    // 2. 检查验证码弹窗
    const captchaModal = document.getElementById('captchaModal');
    if (captchaModal && !captchaModal.classList.contains('hidden')) return 'captcha';
    // 3. 检查结果容器是否可见（resultContainer没有hidden类 = 结果正在展示）
    const resultContainer = document.getElementById('resultContainer');
    if (resultContainer && !resultContainer.classList.contains('hidden')) {
        // 结果容器可见，检查是否有成功数据
        const nameEl = document.querySelector('[data-value="xm"]');
        if (nameEl && nameEl.textContent.trim().length > 0) return 'result_ok';
        return 'result_err';
    }
    // 4. 检查表单是否可用
    const form = document.getElementById('zkzh');
    if (form && !form.disabled) return 'form';
    // 5. 表单存在但被禁用（结果页面中表单被禁用），resultContainer可能被JS隐藏了
    //    这种情况也视为结果页面
    if (form && form.disabled) return 'result_err';
    return 'unknown';
})()"#;

impl BrowserClient {
    /// Poll until JS expression returns true, or timeout.
    /// turbo mode: 20ms interval, normal: 150ms.
    async fn poll_true(&self, page: &Page, js: &str, max_ms: u64) -> bool {
        let interval = if self.turbo { 20u64 } else { 150u64 };
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

    /// Poll until page reaches a result state (captcha, result_ok, result_err, alert) or timeout
    async fn poll_submit_result(&self, page: &Page, max_ms: u64) -> SubmitResult {
        let interval = if self.turbo { 20u64 } else { 150u64 };
        let attempts = (max_ms / interval).max(5);
        for _ in 0..attempts {
            let state: String = page.evaluate_expression(JS_PAGE_STATE)
                .await.map(|r| r.into_value().unwrap_or("unknown".to_string())).unwrap_or("unknown".to_string());
            match state.as_str() {
                "captcha" => return SubmitResult::CaptchaNeeded,
                "result_ok" | "result_err" | "alert" => return SubmitResult::ResultReady,
                _ => {} // "form" or "unknown" - still waiting
            }
            self.sleep_critical(interval).await;
        }
        SubmitResult::Timeout
    }

    pub fn set_perf(&mut self, perf: Option<Arc<Mutex<Vec<crate::data::PerfEvent>>>>) {
        self.perf = perf;
        self.start = std::time::Instant::now();
        self.perf_event("浏览器获取完成");
    }

    pub fn set_turbo(&mut self, turbo: bool) {
        self.turbo = turbo;
    }

    pub fn set_status(&mut self, status: Option<Arc<Mutex<Vec<BrowserStatus>>>>) {
        self.status = status;
    }

    pub fn set_captcha_stats(&mut self, stats: Option<Arc<Mutex<CaptchaStats>>>) {
        self.captcha_stats = stats;
    }

    pub fn instance_id(&self) -> u64 {
        self.instance_id
    }

    fn update_step(&self, step: BrowserStep) {
        // 重置步骤计时器
        if let Ok(mut t) = self.step_start.lock() {
            *t = std::time::Instant::now();
        }
        // 记录步骤开始时的 Unix 时间戳（毫秒），供 UI 端实时计算耗时
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if let Some(st) = &self.status {
            if let Ok(mut statuses) = st.try_lock() {
                if let Some(s) = statuses.iter_mut().find(|s| s.id == self.instance_id) {
                    s.step = step;
                    s.elapsed_ms = 0; // 新步骤从0开始计时
                    s.step_start_ms = now_ms; // UI 端用 now - step_start_ms 实时计算
                }
            }
        }
    }

    /// 刷新当前步骤的耗时显示
    fn refresh_elapsed(&self) {
        let elapsed = self.step_start.lock()
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        if let Some(st) = &self.status {
            if let Ok(mut statuses) = st.try_lock() {
                if let Some(s) = statuses.iter_mut().find(|s| s.id == self.instance_id) {
                    s.elapsed_ms = elapsed;
                }
            }
        }
    }

    fn update_target(&self, target: String) {
        if let Some(st) = &self.status {
            if let Ok(mut statuses) = st.try_lock() {
                if let Some(s) = statuses.iter_mut().find(|s| s.id == self.instance_id) {
                    s.target = target;
                }
            }
        }
    }

    fn update_name(&self, name: String) {
        if let Some(st) = &self.status {
            if let Ok(mut statuses) = st.try_lock() {
                if let Some(s) = statuses.iter_mut().find(|s| s.id == self.instance_id) {
                    s.name = name;
                }
            }
        }
    }

    fn update_captcha_attempt(&self, attempt: u32, max: u32) {
        if let Some(st) = &self.status {
            if let Ok(mut statuses) = st.try_lock() {
                if let Some(s) = statuses.iter_mut().find(|s| s.id == self.instance_id) {
                    s.captcha_attempt = attempt;
                    s.captcha_max = max;
                }
            }
        }
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
            20u64
        } else {
            (self.step_delay_ms as f64 * factor) as u64
        };
        tokio::time::sleep(std::time::Duration::from_millis(ms.max(20))).await;
    }

    async fn sleep_critical(&self, ms: u64) {
        // turbo 模式下将等待时间压缩到最多300ms（从800ms降低）
        let actual = if self.turbo { ms.min(300) } else { ms };
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

        let instance_id = INSTANCE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let user_data_dir = format!("/tmp/chromiumoxide-runner-{}", instance_id);
        let _ = std::fs::create_dir_all(format!("/tmp/gaokao-captcha-{}", instance_id));

        // 核心：勾选"隐藏浏览器"时使用 Chrome New Headless 模式
        // HeadlessMode::New (Chrome 112+) 运行完整浏览器引擎，只是不显示窗口
        // CDP 鼠标事件、Canvas、JS 全部正常工作
        // HeadlessMode::True 是旧无头模式（阉割引擎），网站可能检测到并拒绝响应
        let headless = if hide_browser {
            eprintln!("[Browser#{}] 使用 New Headless 模式（完整引擎 + SwiftShader，无窗口）", instance_id);
            HeadlessMode::New
        } else {
            HeadlessMode::False
        };

        let mut builder = BrowserConfig::builder()
            .chrome_executable(chrome_path)
            .headless_mode(headless)
            .user_data_dir(&user_data_dir)
            // 反自动化检测：隐藏 navigator.webdriver 标志
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--window-size=400,400")
            // 性能优化：防止Chrome后台节流
            .arg("--disable-background-timer-throttling")
            .arg("--disable-backgrounding-occluded-windows")
            .arg("--disable-renderer-backgrounding")
            .arg("--disable-features=CalculateNativeWinOcclusion")
            // 内存优化
            .arg("--disable-dev-shm-usage")
            .arg("--no-sandbox")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-translate")
            .arg("--metrics-recording-only")
            .arg("--no-first-run")
            .arg("--safebrowsing-disable-auto-update");

        // 无头模式使用软件渲染（SwiftShader），不需要禁用 GPU
        // 有头模式在 Linux 上使用 --disable-gpu 避免驱动问题
        if !hide_browser {
            builder = builder.arg("--disable-gpu");
        } else {
            // 无头模式：启用 SwiftShader 软件渲染，确保 Canvas/WebGL 正常工作
            // 这对验证码图片渲染很重要
            builder = builder.arg("--use-gl=angle");
            builder = builder.arg("--use-angle=swiftshader");
        }

        // 无头模式不需要窗口管理参数；有头模式也不需要旧的位置偏移了
        // （New Headless 模式本身就没有窗口）

        let config = builder.build()
            .map_err(|e| format!("浏览器配置失败: {}", e))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| format!("浏览器启动失败: {}", e))?;

        let browser = Arc::new(browser);

        let browser_clone = browser.clone();
        // 修复：handler task 在 stream 结束时自动退出，不再无限循环
        let handler_task = tokio::spawn(async move {
            while let Some(_) = handler.next().await {}
        });

        // New Headless 模式没有窗口，不需要 xdotool/wmctrl 隐藏

        let url = target_url.to_string();
        let page = browser_clone
            .new_page(&url)
            .await
            .map_err(|e| format!("打开页面失败: {}", e))?;

        // ── 反自动化检测注入（关键：每次导航都生效） ──
        // HeadlessMode::New 虽然运行完整引擎，但仍需隐藏自动化痕迹
        // 使用 CDP Page.addScriptToEvaluateOnNewDocument 确保每次页面加载都注入
        {
            use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
            let anti_detect_js = r#"
                // 1. 覆盖 navigator.webdriver（最核心的检测点）
                Object.defineProperty(navigator, 'webdriver', { get: () => undefined });

                // 2. 覆盖 chrome.runtime（headless 下不存在，补充上去）
                if (!window.chrome) window.chrome = {};
                if (!window.chrome.runtime) window.chrome.runtime = { connect: function(){}, sendMessage: function(){} };

                // 3. 覆盖 permissions API 检测（headless 下 notification permission 行为不同）
                const origQuery = window.Permissions?.prototype?.query;
                if (origQuery) {
                    window.Permissions.prototype.query = function(params) {
                        if (params.name === 'notifications') {
                            return Promise.resolve({ state: Notification.permission });
                        }
                        return origQuery.call(this, params);
                    };
                }

                // 4. 覆盖 navigator.plugins（headless 通常为空数组，伪造真实浏览器插件列表）
                //    真实浏览器的 navigator.plugins 是 PluginArray，每个元素有 name/description/filename
                Object.defineProperty(navigator, 'plugins', {
                    get: () => {
                        const fakePlugin = (name, desc, fn) => ({
                            name, description: desc, filename: fn,
                            length: 1, 0: { type: 'application/pdf', suffixes: 'pdf', description: desc }
                        });
                        const arr = [
                            fakePlugin('PDF Viewer', 'Portable Document Format', 'internal-pdf-viewer'),
                            fakePlugin('Chrome PDF Viewer', 'Portable Document Format', 'internal-pdf-viewer'),
                            fakePlugin('Chromium PDF Viewer', 'Portable Document Format', 'internal-pdf-viewer'),
                            fakePlugin('Microsoft Edge PDF Viewer', 'Portable Document Format', 'internal-pdf-viewer'),
                            fakePlugin('WebKit built-in PDF', 'Portable Document Format', 'internal-pdf-viewer'),
                        ];
                        // 模拟 PluginArray 接口
                        arr.item = (i) => arr[i] || null;
                        arr.namedItem = (n) => arr.find(p => p.name === n) || null;
                        arr.refresh = () => {};
                        return arr;
                    }
                });

                // 5. 覆盖 navigator.languages（headless 可能只有 ['en-US']）
                Object.defineProperty(navigator, 'languages', {
                    get: () => ['zh-CN', 'zh', 'en-US', 'en']
                });

                // 6. 覆盖 navigator.platform（headless 在 Linux 上返回 'Linux x86_64'，正常 Chrome 也返回这个，但确保一致性）
                //    不做修改，Linux 上的正常 Chrome 也是这个值

                // 7. 覆盖 WebGL 渲染器检测（某些网站检查 WEBGL_debug_renderer_info）
                const getParameter = WebGLRenderingContext.prototype.getParameter;
                WebGLRenderingContext.prototype.getParameter = function(param) {
                    if (param === 37445) return 'Intel Inc.';  // UNMASKED_VENDOR_WEBGL
                    if (param === 37446) return 'Intel Iris OpenGL Engine';  // UNMASKED_RENDERER_WEBGL
                    return getParameter.call(this, param);
                };
                if (typeof WebGL2RenderingContext !== 'undefined') {
                    const getParameter2 = WebGL2RenderingContext.prototype.getParameter;
                    WebGL2RenderingContext.prototype.getParameter = function(param) {
                        if (param === 37445) return 'Intel Inc.';
                        if (param === 37446) return 'Intel Iris OpenGL Engine';
                        return getParameter2.call(this, param);
                    };
                }

                // 8. 伪造 navigator.connection（部分网站检测网络类型判断是否为机器人）
                if (!navigator.connection) {
                    Object.defineProperty(navigator, 'connection', {
                        get: () => ({ effectiveType: '4g', rtt: 50, downlink: 10, saveData: false })
                    });
                }

                // 9. 覆盖 Chrome 自动化特征：window.cdc_adoQpoasnfa76pfcZLmcfl_Array
                //    这是 ChromeDriver 注入的特征变量
                for (const key in window) {
                    if (key.match(/^cdc_/)) {
                        delete window[key];
                    }
                }

                // 10. 确保 navigator.mimeTypes 不为空（与 plugins 配套）
                Object.defineProperty(navigator, 'mimeTypes', {
                    get: () => {
                        const pdfMime = {
                            type: 'application/pdf',
                            suffixes: 'pdf',
                            description: 'Portable Document Format',
                            enabledPlugin: navigator.plugins[0]
                        };
                        const arr = [pdfMime];
                        arr.item = (i) => arr[i] || null;
                        arr.namedItem = (n) => arr.find(m => m.type === n) || null;
                        return arr;
                    }
                });
            "#;
            let params = AddScriptToEvaluateOnNewDocumentParams::new(anti_detect_js);
            let _ = page.execute(params).await;
        }

        // 修复 Bug 7: 页面加载失败时报错
        let interval = if turbo { 50u64 } else { 200u64 };
        let attempts = 15000 / interval;
        let mut page_ready = false;
        for _ in 0..attempts {
            let ready: bool = page.evaluate_expression(
                r#"(function() {
                    const el = document.getElementById('zkzh');
                    return el !== null;
                })()"#
            ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);
            if ready { page_ready = true; break; }
            tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
        }

        if !page_ready {
            return Err("页面加载超时：未找到表单元素(zkzh)，请检查目标网址是否正确".to_string());
        }

        let now = std::time::Instant::now();
        Ok(Self {
            _browser: browser,
            _handler_task: handler_task,
            page: Arc::new(Mutex::new(page)),
            log,
            perf,
            step_delay_ms,
            captcha_retries,
            captcha_wait_ms,
            target_url: url,
            instance_id,
            start: now,
            step_start: Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
            turbo,
            status: None,
            captcha_stats: None,
        })
    }

    pub async fn go_home(&self) -> Result<(), String> {
        self.update_step(BrowserStep::GoingHome);

        // go_home 整体加超时保护，防止卡死导致浏览器池耗尽
        // turbo模式下缩短超时到10秒
        let timeout_secs = if self.turbo { 10 } else { 20 };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            self.go_home_inner()
        ).await;

        match result {
            Ok(r) => r,
            Err(_) => {
                self.log_msg("go_home超时(20s)，强制刷新页面");
                // 超时后强制刷新页面作为补救
                let page = self.page.lock().await;
                let _ = page.evaluate_expression(r#"window.location.reload()"#).await;
                drop(page);
                // 给页面一点加载时间
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                Ok(())
            }
        }
    }

    async fn go_home_inner(&self) -> Result<(), String> {
        let page = self.page.lock().await;

        // 检测当前页面状态
        let page_state: String = page.evaluate_expression(JS_PAGE_STATE)
            .await.map(|r| r.into_value().unwrap_or("unknown".to_string())).unwrap_or("unknown".to_string());

        // 只有在 "form" 状态下才能用 JS 重置表单（0.1-0.3秒）
        // 其他状态（result_ok, result_err, captcha）都需要完整导航
        if page_state == "form" {
            let reset_ok: bool = page.evaluate_expression(
                r#"(function() {
                    // 关闭所有弹窗
                    const alertBtn = document.getElementById('alertOkButton');
                    if (alertBtn) alertBtn.click();
                    ['captchaModal', 'alertModal'].forEach(id => {
                        const m = document.getElementById(id);
                        if (m) m.classList.add('hidden');
                    });
                    // 清除验证码弹窗中的残留选中状态
                    const captchaModal = document.getElementById('captchaModal');
                    if (captchaModal) {
                        captchaModal.querySelectorAll('.selected, .clicked, .active').forEach(el => {
                            el.classList.remove('selected', 'clicked', 'active');
                        });
                    }
                    // 重置所有输入框
                    document.querySelectorAll('input[type="text"]').forEach(el => {
                        const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
                        setter.call(el, '');
                        el.dispatchEvent(new Event('input', {bubbles: true}));
                        el.dispatchEvent(new Event('change', {bubbles: true}));
                    });
                    return true;
                })()"#
            ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);

            if reset_ok {
                self.perf_event("JS重置表单");
                return Ok(());
            }
        }

        // 非 form 状态（结果页面、验证码页面等），走完整导航
        self.log_msg(&format!("页面状态为[{}]，执行完整页面导航", page_state));

        // goto 加超时保护
        let goto_result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            page.goto(&self.target_url)
        ).await;

        match goto_result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("导航回首页失败: {}", e)),
            Err(_) => return Err("导航回首页超时(15秒)".to_string()),
        }

        let loaded = self.poll_true(&page,
            r#"(function() {
                const el = document.getElementById('zkzh');
                return el !== null;
            })()"#, 8000).await;

        if !loaded {
            return Err("导航回首页超时：表单元素未加载".to_string());
        }

        self.perf_event("导航回首页");
        Ok(())
    }

    pub async fn query_single(
        &self,
        baominghao: &str,
        shenfenzheng: &str,
        name: &str,
    ) -> Result<QueryResult, String> {
        self.perf_event("开始查询");
        self.update_step(BrowserStep::CheckingPage);
        self.update_name(name.to_string());
        self.update_target(baominghao.to_string());

        // 用 tokio::timeout 包裹整个查询，防止卡死
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.query_single_inner(baominghao, shenfenzheng, name)
        ).await;

        match result {
            Ok(r) => r,
            Err(_) => {
                self.update_step(BrowserStep::Error("查询超时(120s)".to_string()));
                Err("查询超时(120秒)，浏览器可能无响应".to_string())
            }
        }
    }

    async fn query_single_inner(
        &self,
        baominghao: &str,
        shenfenzheng: &str,
        name: &str,
    ) -> Result<QueryResult, String> {
        let page = self.page.lock().await;

        // 检查页面是否处于表单状态（只有form状态才能填写和提交）
        let page_state: String = page.evaluate_expression(JS_PAGE_STATE)
            .await.map(|r| r.into_value().unwrap_or("unknown".to_string())).unwrap_or("unknown".to_string());

        self.log_msg(&format!("[{}] 页面状态=[{}], 开始填表 报考号={} 身份证={}...", name, page_state, baominghao, &shenfenzheng[..shenfenzheng.len().min(6)]));

        if page_state != "form" {
            // 非表单状态（结果页面、验证码页面等），导航回首页
            self.log_msg(&format!("页面状态为[{}]，导航回首页...", page_state));

            // goto 加超时保护
            let goto_result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                page.goto(&self.target_url)
            ).await;

            match goto_result {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(format!("导航回首页失败: {}", e)),
                Err(_) => return Err("导航回首页超时(15秒)".to_string()),
            }

            let loaded = self.poll_true(&page,
                r#"(function() {
                    const el = document.getElementById('zkzh');
                    return el !== null;
                })()"#, 8000).await;

            if !loaded {
                return Err("页面未就绪：导航回首页后表单元素仍未加载，请检查目标网址".to_string());
            }
            self.sleep_critical(500).await;
        }

        fn js_fill(id: &str, val: &str) -> String {
            format!(
                "(function(){{const el=document.getElementById('{}');if(!el)return'no_{}';const setter=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value').set;setter.call(el,'{}');el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return'ok';}})()",
                id, id, val
            )
        }

        self.update_step(BrowserStep::FillingForm);
        let fill_result: String = page.evaluate_expression(js_fill("zkzh", baominghao))
            .await.map_err(|e| format!("填报名号失败: {}", e))?
            .into_value().map_err(|_| "填报名号返回值解析失败".to_string())?;
        if fill_result != "ok" {
            return Err("报名号输入框未找到(zkzh)，页面可能未正确加载".to_string());
        }

        let fill_result: String = page.evaluate_expression(js_fill("sfzh", shenfenzheng))
            .await.map_err(|e| format!("填身份证号失败: {}", e))?
            .into_value().map_err(|_| "填身份证返回值解析失败".to_string())?;
        if fill_result != "ok" {
            return Err("身份证输入框未找到(sfzh)，页面可能未正确加载".to_string());
        }

        self.sleep_step(0.15).await;
        self.perf_event("填写信息完成");
        self.update_step(BrowserStep::Submitting);

        let click_result: String = page.evaluate_expression(
            r#"(function() {
                const btn = document.querySelector('button[type="submit"]');
                if (btn) { btn.click(); return 'clicked'; }
                return 'no_button';
            })()"#
        ).await.map(|r| r.into_value().unwrap_or_default()).unwrap_or_default();

        if click_result != "clicked" {
            return Err("提交按钮未找到，页面可能未正确加载".to_string());
        }

        // 提交后轮询：等待验证码弹窗 或 查询结果页面
        self.perf_event("等待验证码或结果");
        self.update_step(BrowserStep::WaitingCaptcha);
        let submit_result = self.poll_submit_result(&page, 15000).await;

        match submit_result {
            SubmitResult::CaptchaNeeded => {
                // 验证码弹窗出现，进入验证码流程
                self.perf_event("验证码弹窗出现");
                if let Err(e) = self.solve_captcha_modal(&page).await {
                    self.perf_event("总耗时");
                    self.update_step(BrowserStep::Error(e.clone()));
                    return Err(format!("验证码处理失败: {}", e));
                }
                // 验证码通过后，等待查询结果加载
                self.update_step(BrowserStep::ReadingResult);
                self.perf_event("等待查询结果");
                // 轮询直到页面离开captcha状态
                let result_ready = self.poll_submit_result(&page, 8000).await;
                match result_ready {
                    SubmitResult::ResultReady => {
                        self.perf_event("查询结果已加载");
                    }
                    SubmitResult::CaptchaNeeded => {
                        // 验证码又弹出来了（可能验证码验证失败后又弹出新验证码）
                        self.log_msg("验证码通过后又出现验证码，尝试再次处理");
                        if let Err(e) = self.solve_captcha_modal(&page).await {
                            self.perf_event("总耗时");
                            self.update_step(BrowserStep::Error(e.clone()));
                            return Err(format!("二次验证码处理失败: {}", e));
                        }
                        // 再等结果
                        let retry_result = self.poll_submit_result(&page, 8000).await;
                        if let SubmitResult::Timeout = retry_result {
                            self.perf_event("总耗时");
                            return Err("二次验证码后等待结果超时".to_string());
                        }
                    }
                    SubmitResult::Timeout => {
                        self.perf_event("总耗时");
                        return Err("验证码通过后等待结果超时".to_string());
                    }
                }
            }
            SubmitResult::ResultReady => {
                // 直接跳到结果页面（无验证码或无需验证码）
                self.update_step(BrowserStep::ReadingResult);
                self.perf_event("直接进入结果页面");
            }
            SubmitResult::Timeout => {
                self.perf_event("总耗时");
                return Err("提交后等待响应超时".to_string());
            }
        }

        // 读取结果：统一处理所有结果状态
        let final_state: String = page.evaluate_expression(JS_PAGE_STATE)
            .await.map(|r| r.into_value().unwrap_or("unknown".to_string())).unwrap_or("unknown".to_string());

        match final_state.as_str() {
            "alert" => {
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
            "result_err" => {
                // "未查询到数据"结果页面，提取页面提示信息
                let no_data_msg: String = page.evaluate_expression(
                    r#"(function() {
                        // resultContainer可见但没有姓名数据，说明是"未查询到数据"
                        const allText = document.body.innerText || '';
                        if (allText.includes('未查询到数据')) return '未查询到数据，请检查输入信息是否正确';
                        if (allText.includes('未找到')) return '未找到查询结果';
                        return '未查询到数据';
                    })()"#
                ).await.map(|r| r.into_value().unwrap_or_default()).unwrap_or_default();

                self.perf_event("总耗时");
                return Err(no_data_msg);
            }
            "result_ok" => {
                // 成功结果，继续读取数据
            }
            _ => {
                self.perf_event("总耗时");
                return Err(format!("无法识别页面状态: {}", final_state));
            }
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
            return Err("未找到查询结果（姓名为空）".to_string());
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
        // 记录上一轮验证码是否因错误而自动刷新了（避免二次刷新）
        let mut captcha_auto_refreshed = false;

        // 统计唯一验证码挑战数（每次进入本函数计一次）
        if let Some(cs) = &self.captcha_stats {
            let mut stats = cs.lock().await;
            stats.total_challenges += 1;
        }

        for attempt in 1..=max_retries {
            self.update_captcha_attempt(attempt, max_retries);
            // 统计验证码尝试次数（每次重试都计一次）
            if let Some(cs) = &self.captcha_stats {
                let mut stats = cs.lock().await;
                stats.total_attempts += 1;
            }

            if attempt > 1 {
                // ── 修复问题2：只在验证码未自动刷新时才手动刷新 ──
                if captcha_auto_refreshed {
                    // 上一轮错误弹窗关闭后网站已自动刷新了验证码，无需再点刷新按钮
                    self.log_msg("验证码已自动刷新，跳过手动刷新");
                    captcha_auto_refreshed = false;
                } else {
                    // 正常手动刷新，增加延迟防止"点击过快"检测
                    self.log_msg("刷新验证码...");
                    self.sleep_critical(500).await;  // 刷新前等500ms，防止频繁操作被检测
                    let _ = page.evaluate_expression(
                        r#"(function() {
                            const btn = document.getElementById('refreshCaptcha');
                            if (btn) { btn.click(); return 'ok'; }
                            return 'no_btn';
                        })()"#
                    ).await;
                }
                // 等待验证码图片加载（增加到3s，确保刷新后图片完全加载稳定）
                self.sleep_critical(300).await;  // 先等300ms让验证码DOM稳定
                let interval = if self.turbo { 80u64 } else { 200u64 };
                for _ in 0..(3000 / interval).max(10) {
                    let has_img: bool = page.evaluate_expression(
                        r#"(function() {
                            const img = document.getElementById('captchaImage');
                            if (!img) return false;
                            if (!img.complete || img.naturalWidth === 0) return false;
                            // 确保src不是svg占位图且足够大（真正加载完成）
                            const src = img.getAttribute('src') || '';
                            return src.length > 100;
                        })()"#
                    ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);
                    if has_img { break; }
                    self.sleep_critical(interval).await;
                }
            }
            self.log_msg(&format!("验证码第 {}/{} 次尝试", attempt, max_retries));
            self.update_step(BrowserStep::LoadingCaptchaImage);

            // 等待验证码图片完全稳定（防止网站加载后自动刷新导致图片变化）
            self.sleep_critical(500).await;

            // Poll for captcha image (5s timeout，确保图片完全加载)
            let img_src: String = {
                let interval = if self.turbo { 50u64 } else { 150u64 };
                let attempts = (5000 / interval).max(10);
                let mut last = String::new();
                for _ in 0..attempts {
                    let src: String = page.evaluate_expression(
                        r#"(function() {
                            const img = document.getElementById('captchaImage');
                            if (!img || !img.complete || img.naturalWidth === 0) return '';
                            const s = img.getAttribute('src') || '';
                            if (s && !s.includes('svg+xml') && s.length > 100) return s;
                            try {
                                const c = document.createElement('canvas');
                                c.width = img.naturalWidth;
                                c.height = img.naturalHeight;
                                const ctx = c.getContext('2d');
                                ctx.drawImage(img, 0, 0);
                                return c.toDataURL('image/png');
                            } catch(e) { return ''; }
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

            // Get container dimensions + viewport position (用于CDP鼠标事件)
            let dims_json: String = page.evaluate_expression(
                r#"(function() {
                    const el = document.getElementById('captchaContainer');
                    if (!el) return JSON.stringify({l:0, t:0, w:300, h:150});
                    const rect = el.getBoundingClientRect();
                    return JSON.stringify({l: rect.left, t: rect.top, w: rect.width, h: rect.height});
                })()"#
            ).await.map(|r| r.into_value().unwrap_or_else(|_| r#"{"l":0,"t":0,"w":300,"h":150}"#.to_string()))
                .unwrap_or_else(|_| r#"{"l":0,"t":0,"w":300,"h":150}"#.to_string());

            let dims: serde_json::Value =
                serde_json::from_str(&dims_json).unwrap_or(serde_json::json!({"l":0,"t":0,"w":300,"h":150}));
            let cl = dims["l"].as_f64().unwrap_or(0.0);
            let ct = dims["t"].as_f64().unwrap_or(0.0);
            let cw = dims["w"].as_f64().unwrap_or(300.0);
            let ch = dims["h"].as_f64().unwrap_or(150.0);

            self.perf_event("获取验证码图片");

            // Solve captcha via OCR
            self.perf_event("OCR开始");
            self.update_step(BrowserStep::OcrProcessing);
            let ocr_result = match ocr::solve_captcha(&temp_path, &expected_chars, cw, ch, self.instance_id).await {
                Ok(r) => {
                    self.perf_event("OCR完成");
                    r
                }
                Err(e) => {
                    self.log_msg(&format!("OCR失败: {}, 准备重试", e));
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

            // Click points — 使用CDP协议模拟真实鼠标事件，每次点击间隔100ms
            self.perf_event("验证码点击开始");
            self.update_step(BrowserStep::ClickingCaptcha);
            for (idx, point) in ocr_result.points.iter().enumerate() {
                let vx = cl + point.x * cw;  // viewport X (CSS像素)
                let vy = ct + point.y * ch;  // viewport Y (CSS像素)

                // 后续点先移动鼠标到目标位置（模拟真实鼠标轨迹）
                if idx > 0 {
                    let move_evt = DispatchMouseEventParams::builder()
                        .r#type(DispatchMouseEventType::MouseMoved)
                        .x(vx).y(vy)
                        .build()
                        .map_err(|e| format!("构建鼠标移动事件失败: {}", e))?;
                    let _ = page.execute(move_evt).await;
                }

                // 鼠标按下
                let press_evt = DispatchMouseEventParams::builder()
                    .r#type(DispatchMouseEventType::MousePressed)
                    .x(vx).y(vy)
                    .button(MouseButton::Left)
                    .click_count(1)
                    .build()
                    .map_err(|e| format!("构建鼠标按下事件失败: {}", e))?;
                let _ = page.execute(press_evt).await;

                // 鼠标释放
                let release_evt = DispatchMouseEventParams::builder()
                    .r#type(DispatchMouseEventType::MouseReleased)
                    .x(vx).y(vy)
                    .button(MouseButton::Left)
                    .click_count(1)
                    .build()
                    .map_err(|e| format!("构建鼠标释放事件失败: {}", e))?;
                let _ = page.execute(release_evt).await;

                // 每次点击之间间隔100ms，防止网站检测
                if idx < ocr_result.points.len() - 1 {
                    self.sleep_critical(100).await;
                }
            }
            // 点击后短暂等待，让页面响应
            self.sleep_critical(100).await;

            // ── 快速检测验证码结果（同时检测alert弹窗、captchaModal消失、以及页面进入结果状态） ──
            self.perf_event("验证码点击完成");
            self.update_step(BrowserStep::WaitingCaptchaResult);
            let max_polls = 40; // 增加到40次轮询（约4秒），确保捕获验证码通过
            let mut captcha_passed = false;
            let mut _alert_found = false;
            for i in 0..max_polls {
                let state: String = page.evaluate_expression(JS_PAGE_STATE)
                    .await.map(|v| v.into_value().unwrap_or("unknown".to_string())).unwrap_or("unknown".to_string());
                match state.as_str() {
                    "captcha" => {
                        // 验证码弹窗还在，继续等待
                    }
                    "alert" => {
                        // 错误弹窗出现
                        _alert_found = true;
                        break;
                    }
                    "result_ok" | "result_err" | "form" => {
                        // 验证码弹窗已消失，页面进入了结果或表单状态
                        captcha_passed = true;
                        break;
                    }
                    _ => {
                        // unknown状态，可能验证码刚消失，继续等
                    }
                }
                // 前5次快速轮询（50ms），之后放慢到200ms
                let delay = if i < 5 { 50 } else { 200 };
                self.sleep_critical(delay).await;
            }

            if captcha_passed {
                // Captcha passed
                self.log_msg("验证码通过");
                self.perf_event("验证码验证通过");
                if let Some(cs) = &self.captcha_stats {
                    let mut stats = cs.lock().await;
                    stats.total_passes += 1;
                    if attempt == 1 {
                        stats.first_try_passes += 1;
                    }
                }
                return Ok(());
            }

            // ── 快速关闭alert弹窗，彻底清理页面状态 ──
            self.log_msg("验证码验证失败，快速关闭弹窗");
            self.update_step(BrowserStep::DismissingAlert);

            // 立即关闭alert弹窗
            let dismiss_result: String = page.evaluate_expression(
                r#"(function() {
                    // 第一步：立即关闭 alert 弹窗
                    const okBtn = document.getElementById('alertOkButton');
                    if (okBtn) { okBtn.click(); }
                    // 第二步：彻底清理页面状态，重置 captchaModal 中的已选状态
                    const captchaModal = document.getElementById('captchaModal');
                    if (captchaModal) {
                        captchaModal.querySelectorAll('.selected, .clicked, .active').forEach(el => {
                            el.classList.remove('selected', 'clicked', 'active');
                        });
                    }
                    // 第三步：确保 alertModal 也被关闭
                    const alertModal = document.getElementById('alertModal');
                    if (alertModal) { alertModal.classList.add('hidden'); }
                    return okBtn ? 'alert_dismissed' : 'no_alert';
                })()"#
            ).await.map(|v| v.into_value().unwrap_or_default()).unwrap_or_default();

            self.log_msg(&format!("弹窗处理: {}", dismiss_result));

            // 短暂等待让网站完成自动刷新验证码
            self.sleep_critical(300).await;

            // 检查网站是否自动刷新了验证码（检查captchaModal是否仍可见且验证码图片已就绪）
            // 只在图片已完全加载时标记为自动刷新，避免误判
            let auto_refreshed: bool = page.evaluate_expression(
                r#"(function() {
                    // 验证码弹窗必须仍可见
                    const m = document.getElementById('captchaModal');
                    if (!m || m.classList.contains('hidden')) return false;
                    // 验证码图片必须已加载
                    const img = document.getElementById('captchaImage');
                    if (!img || !img.complete || img.naturalWidth === 0) return false;
                    // 图片src必须是有效的（非svg占位图）
                    const src = img.getAttribute('src') || '';
                    return src.length > 100;
                })()"#
            ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);

            captcha_auto_refreshed = auto_refreshed;

            // 确保captchaModal仍然可见且可用
            let modal_ok: bool = page.evaluate_expression(
                r#"(function() {
                    const m = document.getElementById('captchaModal');
                    const img = document.getElementById('captchaImage');
                    return m ? (!m.classList.contains('hidden') && img !== null) : false;
                })()"#
            ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);

            if !modal_ok {
                // captchaModal 消失了或不可用，检查页面状态
                let state: String = page.evaluate_expression(JS_PAGE_STATE)
                    .await.map(|r| r.into_value().unwrap_or("unknown".to_string())).unwrap_or("unknown".to_string());
                if state != "captcha" {
                    // 页面已经不在验证码状态了，可能已经进入了结果页面
                    self.log_msg(&format!("验证码弹窗已消失，页面状态: {}", state));
                    // 如果已经是结果状态，说明验证码实际上通过了
                    if state == "result_ok" || state == "result_err" {
                        self.log_msg("验证码实际已通过，页面已进入结果状态");
                        if let Some(cs) = &self.captcha_stats {
                            let mut stats = cs.lock().await;
                            stats.total_passes += 1;
                            if attempt == 1 {
                                stats.first_try_passes += 1;
                            }
                        }
                        return Ok(());
                    }
                    return Err("验证码弹窗状态异常，无法恢复".to_string());
                }
                // 仍然是captcha状态但modal不可用，等待恢复
                self.log_msg("验证码弹窗状态异常，等待恢复...");
                let recovered = self.poll_true(&page,
                    r#"(function() {
                        const m = document.getElementById('captchaModal');
                        return m ? !m.classList.contains('hidden') : false;
                    })()"#, 3000).await;
                if !recovered {
                    return Err("验证码弹窗状态异常，无法恢复".to_string());
                }
            }
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

    /// 强制关闭浏览器：abort handler task + drop Browser（触发 Chrome 进程退出）
    pub fn shutdown(self) {
        self._handler_task.abort();
        drop(self._browser);
        drop(self.page);
    }
}

pub struct BrowserPool {
    clients: Mutex<VecDeque<BrowserClient>>,
    semaphore: Arc<Semaphore>,
    /// 标记是否已关闭，防止 release 回写已关闭的池
    shutdown_flag: AtomicBool,
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
            shutdown_flag: AtomicBool::new(false),
        }))
    }

    /// 获取池中所有浏览器的 instance_id（用于初始化 browser_statuses）
    pub async fn instance_ids(&self) -> Vec<u64> {
        let clients = self.clients.lock().await;
        clients.iter().map(|c| c.instance_id).collect()
    }

    /// 强制关闭所有浏览器并标记池已关闭
    pub fn force_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
    }

    /// 池是否已关闭
    pub fn is_shutdown(&self) -> bool {
        self.shutdown_flag.load(Ordering::SeqCst)
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<(OwnedSemaphorePermit, BrowserClient), String> {
        let permit = self.semaphore.clone().acquire_owned().await.map_err(|_| "信号量获取失败".to_string())?;
        let mut clients = self.clients.lock().await;
        let client = clients.pop_front().ok_or_else(|| "浏览器池耗尽：没有可用的浏览器实例".to_string())?;
        // 如果池已关闭，获取到的浏览器也要标记（不过正常流程中 acquire 前会检查 cancel_flag）
        Ok((permit, client))
    }

    pub fn release(self: &Arc<Self>, permit: OwnedSemaphorePermit, client: BrowserClient) {
        // 如果池已关闭，直接杀掉浏览器而不是放回池中
        if self.is_shutdown() {
            client.shutdown();
            drop(permit);
            return;
        }

        let this = self.clone();
        tokio::spawn(async move {
            // 检查池是否在 go_home 期间被关闭
            if this.is_shutdown() {
                client.shutdown();
                drop(permit);
                return;
            }

            // go_home 加了超时保护，不会无限卡住
            if let Err(_e) = client.go_home().await {
                // go_home 失败，强制刷新页面
                let page = client.page.lock().await;
                let _ = page.evaluate_expression(
                    r#"window.location.reload()"#
                ).await;
                drop(page);
                tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
            }

            // 再次检查池是否已关闭
            if this.is_shutdown() {
                client.shutdown();
                drop(permit);
                return;
            }

            // 重置状态为空闲
            if let Some(st) = &client.status {
                if let Ok(mut statuses) = st.try_lock() {
                    if let Some(s) = statuses.iter_mut().find(|s| s.id == client.instance_id) {
                        s.step = BrowserStep::Idle;
                        s.target = String::new();
                        s.name = String::new();
                        s.captcha_attempt = 0;
                        s.captcha_max = 0;
                        s.elapsed_ms = 0;
                        s.step_start_ms = 0; // 停止计时
                    }
                }
            }
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
