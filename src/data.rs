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
    pub total: usize,
    pub matched: usize,
    pub not_found: usize,
    pub current_name: String,
    pub current_exam: String,
    pub current_batch: String,
}

#[derive(Debug, Clone)]
pub struct PerfEvent {
    pub label: &'static str,
    pub elapsed_ms: u64,
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
