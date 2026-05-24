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
    pub concurrency: u32,
    pub delay_ms: u32,
    pub step_delay_ms: u32,
    pub captcha_retries: u32,
    pub hide_browser: bool,
    pub debug_mode: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            baokao_path: String::new(),
            sfz_path: String::new(),
            concurrency: 1,
            delay_ms: 2000,
            step_delay_ms: 1000,
            captcha_retries: 5,
            hide_browser: true,
            debug_mode: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QueryStatus {
    Pending,
    Querying,
    Success,
    Failed(String),
}
