use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, OwnedSemaphorePermit};
use chromiumoxide::{
    browser::{Browser, BrowserConfig, HeadlessMode},
    Page,
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
    /// turbo mode: 30ms interval, normal: 150ms.
    async fn poll_true(&self, page: &Page, js: &str, max_ms: u64) -> bool {
        let interval = if self.turbo { 30u64 } else { 150u64 };
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
        let interval = if self.turbo { 30u64 } else { 150u64 };
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
        if let Some(st) = &self.status {
            if let Ok(mut statuses) = st.try_lock() {
                if let Some(s) = statuses.iter_mut().find(|s| s.id == self.instance_id) {
                    s.step = step;
                    s.elapsed_ms = self.start.elapsed().as_millis() as u64;
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
        // turbo 模式下将等待时间压缩到最多800ms
        let actual = if self.turbo { ms.min(800) } else { ms };
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
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--window-size=400,400");

        if hide_browser {
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
            status: None,
            captcha_stats: None,
        })
    }

    pub async fn go_home(&self) -> Result<(), String> {
        self.update_step(BrowserStep::GoingHome);
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
        page.goto(&self.target_url)
            .await
            .map_err(|e| format!("导航回首页失败: {}", e))?;

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
        let page = self.page.lock().await;

        // 检查页面是否处于表单状态（只有form状态才能填写和提交）
        let page_state: String = page.evaluate_expression(JS_PAGE_STATE)
            .await.map(|r| r.into_value().unwrap_or("unknown".to_string())).unwrap_or("unknown".to_string());

        if page_state != "form" {
            // 非表单状态（结果页面、验证码页面等），导航回首页
            self.log_msg(&format!("页面状态为[{}]，导航回首页...", page_state));
            page.goto(&self.target_url)
                .await
                .map_err(|e| format!("导航回首页失败: {}", e))?;

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

        self.sleep_step(0.3).await;
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

        for attempt in 1..=max_retries {
            self.update_captcha_attempt(attempt, max_retries);
            // 统计验证码尝试次数
            if let Some(cs) = &self.captcha_stats {
                if let Ok(mut stats) = cs.try_lock() {
                    stats.total_attempts += 1;
                }
            }

            if attempt > 1 {
                // ── 修复问题2：只在验证码未自动刷新时才手动刷新 ──
                if captcha_auto_refreshed {
                    // 上一轮错误弹窗关闭后网站已自动刷新了验证码，无需再点刷新按钮
                    self.log_msg("验证码已自动刷新，跳过手动刷新");
                    captcha_auto_refreshed = false;
                } else {
                    // 正常手动刷新
                    self.log_msg("刷新验证码...");
                    let _ = page.evaluate_expression(
                        r#"(function() {
                            const btn = document.getElementById('refreshCaptcha');
                            if (btn) { btn.click(); return 'ok'; }
                            return 'no_btn';
                        })()"#
                    ).await;
                }
                // 等待验证码图片加载（最多1.5s，缩短等待时间）
                let interval = if self.turbo { 80u64 } else { 200u64 };
                for _ in 0..(1500 / interval).max(5) {
                    let has_img: bool = page.evaluate_expression(
                        r#"!!document.getElementById('captchaImage')"#
                    ).await.map(|r| r.into_value().unwrap_or(false)).unwrap_or(false);
                    if has_img { break; }
                    self.sleep_critical(interval).await;
                }
            }
            self.log_msg(&format!("验证码第 {}/{} 次尝试", attempt, max_retries));
            self.update_step(BrowserStep::LoadingCaptchaImage);

            // Poll for captcha image (3s timeout, 缩短到3s)
            let img_src: String = {
                let interval = if self.turbo { 50u64 } else { 150u64 };
                let attempts = (3000 / interval).max(10);
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

            // Click points — 合并为1次JS调用
            self.perf_event("验证码点击开始");
            self.update_step(BrowserStep::ClickingCaptcha);
            let clicks_js = format!(
                r#"(function() {{
                    const container = document.getElementById('captchaContainer');
                    if (!container) return 'no_container';
                    const rect = container.getBoundingClientRect();
                    const points = [{}];
                    points.forEach(([rx, ry], i) => {{
                        setTimeout(() => {{
                            const x = rect.left + rx;
                            const y = rect.top + ry;
                            container.dispatchEvent(new MouseEvent('click', {{
                                clientX: x, clientY: y, bubbles: true, cancelable: true
                            }}));
                        }}, i * 150);
                    }});
                    return 'clicked_all';
                }})()"#,
                ocr_result.points.iter()
                    .map(|p| format!("[{}, {}]", p.x * cw, p.y * ch))
                    .collect::<Vec<_>>().join(", ")
            );
            let _ = page.evaluate_expression(&clicks_js).await;
            self.sleep_critical(300).await;

            // ── 快速检测验证码结果（同时检测alert弹窗、captchaModal消失、以及页面进入结果状态） ──
            self.perf_event("验证码点击完成");
            self.update_step(BrowserStep::WaitingCaptchaResult);
            let max_polls = 15; // 最多约1.5秒
            let mut captcha_passed = false;
            let mut alert_found = false;
            for i in 0..max_polls {
                let state: String = page.evaluate_expression(JS_PAGE_STATE)
                    .await.map(|v| v.into_value().unwrap_or("unknown".to_string())).unwrap_or("unknown".to_string());
                match state.as_str() {
                    "captcha" => {
                        // 验证码弹窗还在，继续等待
                    }
                    "alert" => {
                        // 错误弹窗出现
                        alert_found = true;
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
                // 前几轮快速轮询，后面稍慢
                self.sleep_critical(if i < 3 { 50 } else { 120 }).await;
            }

            if captcha_passed {
                // Captcha passed
                self.log_msg("验证码通过");
                self.perf_event("验证码验证通过");
                if let Some(cs) = &self.captcha_stats {
                    if let Ok(mut stats) = cs.try_lock() {
                        stats.total_passes += 1;
                        if attempt == 1 {
                            stats.first_try_passes += 1;
                        }
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
            self.sleep_critical(400).await;

            // 检查网站是否自动刷新了验证码
            let auto_refreshed: bool = page.evaluate_expression(
                r#"(function() {
                    const img = document.getElementById('captchaImage');
                    if (img && img.complete && img.naturalWidth > 0) return true;
                    const btn = document.getElementById('refreshCaptcha');
                    return btn !== null;
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
                            if let Ok(mut stats) = cs.try_lock() {
                                stats.total_passes += 1;
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
            if let Err(_e) = client.go_home().await {
                // go_home 失败，完整刷新页面作为补救
                let page = client.page.lock().await;
                let _ = page.evaluate_expression(
                    r#"window.location.reload()"#
                ).await;
                drop(page);
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
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
