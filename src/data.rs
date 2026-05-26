use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    pub kemumingcheng: String,      // 科类名称（从网站获取）
    pub kaodianmingcheng: String,   // 考点名称（从网站获取）
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
    pub phase: String,
    pub total_queries: u64,
}

#[derive(Debug, Clone)]
pub struct PerfEvent {
    pub label: &'static str,
    pub elapsed_ms: u64,
}

/// 验证码统计
#[derive(Debug, Clone, Default)]
pub struct CaptchaStats {
    pub total_attempts: u64,
    pub total_passes: u64,
    pub first_try_passes: u64,
    pub total_queries: u64,
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
    pub id: u64,
    pub step: BrowserStep,
    pub target: String,
    pub name: String,
    pub captcha_attempt: u32,
    pub captcha_max: u32,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserStep {
    Idle,
    Acquiring,
    CheckingPage,
    FillingForm,
    Submitting,
    WaitingCaptcha,
    LoadingCaptchaImage,
    OcrProcessing,
    ClickingCaptcha,
    WaitingCaptchaResult,
    CaptchaFailed,
    DismissingAlert,
    ReadingResult,
    GoingHome,
    Error(String),
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
            BrowserStep::Idle => (128, 128, 128),
            BrowserStep::Acquiring => (100, 149, 237),
            BrowserStep::CheckingPage => (100, 149, 237),
            BrowserStep::FillingForm => (65, 105, 225),
            BrowserStep::Submitting => (65, 105, 225),
            BrowserStep::WaitingCaptcha => (255, 165, 0),
            BrowserStep::LoadingCaptchaImage => (255, 165, 0),
            BrowserStep::OcrProcessing => (255, 140, 0),
            BrowserStep::ClickingCaptcha => (255, 140, 0),
            BrowserStep::WaitingCaptchaResult => (255, 165, 0),
            BrowserStep::CaptchaFailed => (255, 69, 0),
            BrowserStep::DismissingAlert => (255, 99, 71),
            BrowserStep::ReadingResult => (50, 205, 50),
            BrowserStep::GoingHome => (147, 112, 219),
            BrowserStep::Error(_) => (255, 0, 0),
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

// ═══════════════════════════════════════════════════════════
//  新增：任务队列 + 班级感知锚点扩展 相关数据结构
// ═══════════════════════════════════════════════════════════

/// 学生信息（含班级号）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudentInfo {
    pub name: String,
    pub sfz: String,
    pub class_num: u32,
}

/// 查询任务类型
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskType {
    SeedProbe,      // 种子探测：用两班学生撞击种子号码
    ClassExpand,    // 班级扩展：从锚点向两侧扩展
    ClassScan,      // 班级扫描：在班级区域内扫描剩余学生
    Cleanup,        // 残留清扫：处理最后未匹配的学生
}

/// 单个查询任务
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryTask {
    pub exam_number: u64,
    pub student_sfz: String,
    pub student_name: String,
    pub class_num: u32,
    pub task_type: TaskType,
}

/// 任务批次（给一个工人的工作量）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBatch {
    pub tasks: Vec<QueryTask>,
    pub batch_id: u32,
}

/// 单个任务的执行结果
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub exam_number: u64,
    pub student_sfz: String,
    pub student_name: String,
    pub class_num: u32,
    pub task_type: TaskType,
    pub matched: bool,
    pub error: String,
    pub kemumingcheng: String,      // 科类名称（从网站获取）
    pub kaodianmingcheng: String,   // 考点名称（从网站获取）
}

/// 班级锚点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anchor {
    pub exam_number: u64,
    pub student_name: String,
    pub student_sfz: String,
    pub class_num: u32,
}

/// 已发现的班级区域
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassZone {
    pub class_num: u32,
    pub start_number: u64,
    pub end_number: u64,
    pub matched_count: usize,
    pub total_count: usize,
}

impl ClassZone {
    pub fn contains(&self, number: u64) -> bool {
        number >= self.start_number && number <= self.end_number
    }

    /// 扩展区域
    pub fn expand_to_include(&mut self, number: u64) {
        if number < self.start_number {
            self.start_number = number;
        }
        if number > self.end_number {
            self.end_number = number;
        }
    }
}

/// 已匹配的学生-考号对
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedPair {
    pub name: String,
    pub sfz: String,
    pub exam_number: u64,
    pub class_num: u32,
    pub kemumingcheng: String,      // 科类名称（从网站获取）
    pub kaodianmingcheng: String,   // 考点名称（从网站获取）
}

/// 扫描阶段
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScanPhase {
    PairSeed,           // 两班种子：用2个班的学生撞5个种子号码
    PairExpand,         // 两班扩展：在锚点附近扩展搜索
    PairScan,           // 两班扫描：在确认的班级区域内扫描
    Cleanup,            // 残留清扫：处理最后未匹配的学生
    Completed,          // 完成
}

impl ScanPhase {
    pub fn label(&self) -> &str {
        match self {
            ScanPhase::PairSeed => "两班种子",
            ScanPhase::PairExpand => "两班扩展",
            ScanPhase::PairScan => "两班扫描",
            ScanPhase::Cleanup => "残留清扫",
            ScanPhase::Completed => "已完成",
        }
    }
}

/// 任务状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JobStatus {
    Running,
    Paused,
    Completed,
}

/// 持久化的推算任务
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionJob {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: JobStatus,

    // 输入参数
    pub target_url: String,
    pub start_bkh: u64,
    pub end_bkh: u64,
    pub sfz_file_hash: String,
    pub bkh_file_hash: String,
    pub year_filter: u32,

    // 进度
    pub total_students: usize,
    pub matched_count: usize,

    // 核心数据
    pub matched_pairs: Vec<MatchedPair>,
    pub unmatched_students: Vec<StudentInfo>,
    pub scanned_numbers: HashSet<u64>,

    // 算法阶段
    pub phase: ScanPhase,
    pub anchors: Vec<Anchor>,
    pub class_zones: Vec<ClassZone>,
    pub seed_cursor: u64,       // 种子扫描游标（持久化，恢复时不再重置）

    // 两班递进扫描状态
    pub class_pair_idx: usize,       // 当前处理的班级对索引
    pub pair_cursor: u64,            // 当前对的搜索起点（end_bkh递减）
    pub pair_round: usize,           // 当前对内第几轮（0=种子,1=扩展,2=扫描）
    pub completed_class_nums: Vec<u32>,  // 已完成的班级号列表

    // 统计
    pub total_queries: u64,
}

impl PredictionJob {
    pub fn new(
        name: String,
        target_url: String,
        start_bkh: u64,
        end_bkh: u64,
        sfz_file_hash: String,
        bkh_file_hash: String,
        year_filter: u32,
        students: Vec<StudentInfo>,
    ) -> Self {
        let total_students = students.len();
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let id = format!("job_{}", chrono::Local::now().format("%Y%m%d_%H%M%S"));

        Self {
            id,
            name,
            created_at: now.clone(),
            updated_at: now,
            status: JobStatus::Paused,
            target_url,
            start_bkh,
            end_bkh,
            sfz_file_hash,
            bkh_file_hash,
            year_filter,
            total_students,
            matched_count: 0,
            matched_pairs: Vec::new(),
            unmatched_students: students,
            scanned_numbers: HashSet::new(),
            phase: ScanPhase::PairSeed,
            anchors: Vec::new(),
            class_zones: Vec::new(),
            seed_cursor: end_bkh,
            class_pair_idx: 0,
            pair_cursor: end_bkh,
            pair_round: 0,
            completed_class_nums: Vec::new(),
            total_queries: 0,
        }
    }

    /// 获取指定班级的未匹配学生
    pub fn unmatched_of_class(&self, class_num: u32) -> Vec<&StudentInfo> {
        self.unmatched_students.iter()
            .filter(|s| s.class_num == class_num)
            .collect()
    }

    /// 获取所有存在的班级号（从未匹配学生中提取）
    pub fn active_classes(&self) -> Vec<u32> {
        let mut classes: Vec<u32> = self.unmatched_students.iter()
            .map(|s| s.class_num)
            .filter(|&c| c > 0)
            .collect();
        classes.sort();
        classes.dedup();
        classes
    }

    /// 获取已有锚点的班级号
    pub fn anchored_classes(&self) -> Vec<u32> {
        let mut classes: Vec<u32> = self.anchors.iter()
            .map(|a| a.class_num)
            .collect();
        classes.sort();
        classes.dedup();
        classes
    }

    /// 获取尚未有锚点的班级号
    pub fn unanchored_classes(&self) -> Vec<u32> {
        let anchored = self.anchored_classes();
        self.active_classes().into_iter()
            .filter(|c| !anchored.contains(c))
            .collect()
    }

    /// 标记一个匹配
    pub fn record_match(&mut self, name: &str, sfz: &str, exam_number: u64, class_num: u32,
                        kemumingcheng: &str, kaodianmingcheng: &str) {
        // 从未匹配列表移除
        self.unmatched_students.retain(|s| s.sfz != sfz);

        // 添加到已匹配列表
        self.matched_pairs.push(MatchedPair {
            name: name.to_string(),
            sfz: sfz.to_string(),
            exam_number,
            class_num,
            kemumingcheng: kemumingcheng.to_string(),
            kaodianmingcheng: kaodianmingcheng.to_string(),
        });

        self.matched_count = self.matched_pairs.len();

        // 添加锚点
        if !self.anchors.iter().any(|a| a.student_sfz == sfz) {
            self.anchors.push(Anchor {
                exam_number,
                student_name: name.to_string(),
                student_sfz: sfz.to_string(),
                class_num,
            });
        }

        // 更新或创建班级区域
        if let Some(zone) = self.class_zones.iter_mut().find(|z| z.class_num == class_num) {
            zone.expand_to_include(exam_number);
            zone.matched_count += 1;
        } else {
            self.class_zones.push(ClassZone {
                class_num,
                start_number: exam_number,
                end_number: exam_number,
                matched_count: 1,
                total_count: 0, // 暂时未知
            });
        }

        self.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    }

    /// 获取进度百分比
    pub fn progress_pct(&self) -> f64 {
        if self.total_students == 0 { 0.0 }
        else { self.matched_count as f64 / self.total_students as f64 * 100.0 }
    }

    /// 获取简要描述
    pub fn summary(&self) -> String {
        format!(
            "{}/{} 匹配 ({:.0}%) | {} | {}",
            self.matched_count,
            self.total_students,
            self.progress_pct(),
            self.phase.label(),
            match self.status {
                JobStatus::Running => "运行中",
                JobStatus::Paused => "暂停",
                JobStatus::Completed => "完成",
            }
        )
    }
}
