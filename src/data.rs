use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct BaoKaoHaoRecord {
    pub xuhao: Option<f64>,
    pub baominghao: String,
    pub name: String,
    pub yuzhong: String,
    pub kouyu: String,
    pub leibie: String,
}

#[derive(Debug, Clone)]
pub struct ShenFenZhengRecord {
    pub shenfenzheng: String,
    pub password: String,
    pub bianhao: String,
    pub name: String,
    pub gender: String,
    pub birth: String,
    pub organization: String,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub ruxue_year: Option<f64>,
    pub minzu: String,
    pub zhengzhi: Option<String>,
    pub wenhua: Option<String>,
    pub zongjiao: Option<String>,
    pub hunyin: Option<String>,
    pub xueji: Option<String>,
    pub zhuanye: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MatchedRecord {
    pub name: String,
    pub baominghao: String,
    pub shenfenzheng_candidates: Vec<String>,
    pub baokao_info: String,
    pub status: MatchStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MatchStatus {
    Pending,
    Matched(String),
    NotFound,
    Multiple,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub name: String,
    pub baominghao: String,
    pub shenfenzheng: String,
    pub kemumingcheng: String,
    pub kaodianmingcheng: String,
    pub status: QueryStatus,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub baokao_path: String,
    pub sfz_path: String,
    pub pred_sfz_path: String,
    pub pred_bkh_path: String,
    pub pred_start_bkh: String,
    pub pred_end_bkh: String,
    pub target_url: String,
    pub concurrency: u32,
    pub delay_ms: u32,
    pub step_delay_ms: u32,
    pub captcha_retries: u32,
    pub captcha_wait_ms: u32,
    pub hide_browser: bool,
    pub debug_mode: bool,
    pub turbo: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            baokao_path: String::new(),
            sfz_path: String::new(),
            pred_sfz_path: String::new(),
            pred_bkh_path: String::new(),
            pred_start_bkh: String::new(),
            pred_end_bkh: String::new(),
            target_url: "https://cx.hbea.edu.cn/gkkd/2026/eb3f6190-590c-4f79-9b88-81a1d0aa0a2b".into(),
            concurrency: 3,
            delay_ms: 2000,
            step_delay_ms: 1000,
            captcha_retries: 5,
            captcha_wait_ms: 2000,
            hide_browser: true,
            debug_mode: false,
            turbo: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Scene {
    ExamRoomQuery,
    NumberPrediction,
}

impl Scene {
    pub fn name(&self) -> &str {
        match self {
            Scene::ExamRoomQuery => "考场查询",
            Scene::NumberPrediction => "报考号推算",
        }
    }
    pub fn all() -> Vec<Scene> {
        vec![Scene::ExamRoomQuery, Scene::NumberPrediction]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QueryStatus {
    Pending,
    Querying,
    Success,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct PredictedRecord {
    pub name: String,
    pub shenfenzheng: String,
    pub exam_number: String,
    pub status: PredictedStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PredictedStatus {
    Matched,
    NotFound,
}

#[derive(Debug, Clone, Default)]
pub struct PredictionProgress {
    pub total: usize,       // 当前班级总人数
    pub matched: usize,     // 已成功匹配人数
    pub not_found: usize,   // 扫射完仍未找到的人数
    pub current_name: String,
    pub current_exam: String,
    pub current_batch: String, // 用于 UI 实时显示："[雷达阶段] 正在探测考号: ..." 或 "[扫射阶段] ..."
}

#[derive(Debug, Clone)]
pub struct PerfEvent {
    pub label: &'static str,
    pub elapsed_ms: u64,
}

/// 验证码统计：精确追踪每次验证码尝试
#[derive(Debug, Clone, Default)]
pub struct CaptchaStats {
    pub total_attempts: u64,   // 总验证码尝试次数
    pub total_passes: u64,     // 总通过次数
    pub first_try_passes: u64, // 首次即通过次数
    pub total_queries: u64,    // 总查询次数（含无需验证码的）
}

impl CaptchaStats {
    pub fn pass_rate(&self) -> f64 {
        if self.total_attempts == 0 { 0.0 } else { self.total_passes as f64 / self.total_attempts as f64 * 100.0 }
    }
    pub fn first_try_rate(&self) -> f64 {
        if self.total_attempts == 0 { 0.0 } else { self.first_try_passes as f64 / self.total_attempts as f64 * 100.0 }
    }
}

/// 单个浏览器的实时状态
#[derive(Debug, Clone)]
pub struct BrowserStatus {
    pub id: u64,                    // 浏览器实例 ID
    pub step: BrowserStep,          // 当前步骤
    pub target: String,             // 当前操作目标（报名号）
    pub name: String,               // 当前查询人姓名
    pub captcha_attempt: u32,       // 当前验证码第几次尝试
    pub captcha_max: u32,           // 验证码最大尝试次数
    pub elapsed_ms: u64,            // 当前步骤已耗时(ms)
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserStep {
    Idle,                       // 空闲等待
    Acquiring,                  // 获取浏览器实例
    CheckingPage,               // 检查页面就绪
    FillingForm,                // 填写表单
    Submitting,                 // 提交查询
    WaitingCaptcha,             // 等待验证码弹窗
    LoadingCaptchaImage,        // 加载验证码图片
    OcrProcessing,              // OCR识别中
    ClickingCaptcha,            // 点击验证码
    WaitingCaptchaResult,       // 等待验证码结果
    CaptchaFailed,              // 验证码失败，准备重试
    DismissingAlert,            // 关闭错误弹窗
    ReadingResult,              // 读取查询结果
    GoingHome,                  // 导航回首页
    Error(String),              // 出错
}

impl BrowserStep {
    pub fn label(&self) -> &str {
        match self {
            BrowserStep::Idle => "⏳ 空闲",
            BrowserStep::Acquiring => "🔄 获取浏览器",
            BrowserStep::CheckingPage => "🔍 检查页面",
            BrowserStep::FillingForm => "✏️ 填写表单",
            BrowserStep::Submitting => "📤 提交查询",
            BrowserStep::WaitingCaptcha => "⏳ 等待验证码",
            BrowserStep::LoadingCaptchaImage => "🖼️ 加载验证码图",
            BrowserStep::OcrProcessing => "🧠 OCR识别中",
            BrowserStep::ClickingCaptcha => "👆 点击验证码",
            BrowserStep::WaitingCaptchaResult => "⏳ 验证码验证中",
            BrowserStep::CaptchaFailed => "❌ 验证码失败",
            BrowserStep::DismissingAlert => "🗑️ 关闭弹窗",
            BrowserStep::ReadingResult => "📋 读取结果",
            BrowserStep::GoingHome => "🏠 回首页",
            BrowserStep::Error(_e) => "❌ 出错",
        }
    }

    pub fn color(&self) -> (u8, u8, u8) {
        match self {
            BrowserStep::Idle => (128, 128, 128),         // 灰色
            BrowserStep::Acquiring => (100, 149, 237),     // 蓝色
            BrowserStep::CheckingPage => (100, 149, 237),
            BrowserStep::FillingForm => (65, 105, 225),
            BrowserStep::Submitting => (65, 105, 225),
            BrowserStep::WaitingCaptcha => (255, 165, 0),  // 橙色
            BrowserStep::LoadingCaptchaImage => (255, 165, 0),
            BrowserStep::OcrProcessing => (255, 140, 0),
            BrowserStep::ClickingCaptcha => (255, 140, 0),
            BrowserStep::WaitingCaptchaResult => (255, 165, 0),
            BrowserStep::CaptchaFailed => (255, 69, 0),    // 红橙
            BrowserStep::DismissingAlert => (255, 99, 71),  // 番茄色
            BrowserStep::ReadingResult => (50, 205, 50),   // 绿色
            BrowserStep::GoingHome => (147, 112, 219),     // 紫色
            BrowserStep::Error(_) => (255, 0, 0),          // 红色
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PerfRecord {
    pub label: &'static str,
    pub times_ms: Vec<u64>,
}

impl PerfRecord {
    pub fn count(&self) -> usize { self.times_ms.len() }
    pub fn avg_ms(&self) -> f64 {
        let n = self.times_ms.len();
        if n == 0 { 0.0 } else { self.times_ms.iter().sum::<u64>() as f64 / n as f64 }
    }
    pub fn max_ms(&self) -> u64 { self.times_ms.iter().copied().fold(0, u64::max) }
    pub fn min_ms(&self) -> u64 { self.times_ms.iter().copied().fold(u64::MAX, u64::min) }
}
