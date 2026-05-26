use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::browser::BrowserPool;
use crate::data::{
    PredictionProgress, PerfEvent, BrowserStatus, CaptchaStats,
    QueryTask, TaskBatch, TaskResult, TaskType, PredictionJob,
    StudentInfo, ScanPhase, Anchor,
    PredictedRecord, PredictedStatus,
};

/// 批次大小：每个工人每次领取的任务数
const BATCH_SIZE: usize = 10;

/// 班级扩展时，每个号码最多尝试的同班学生数
const EXPAND_MAX_TRIES_PER_NUMBER: usize = 50;

/// 种子探测时，每个号码尝试的学生数
const SEED_PROBE_STUDENTS_PER_NUMBER: usize = 10;

/// 班级探测时，每个代表学生尝试的号码范围
const CLASS_PROBE_RANGE: u64 = 200;

// ═══════════════════════════════════════════════════════════
//  任务调度器：根据当前阶段生成任务
// ═══════════════════════════════════════════════════════════

struct TaskScheduler {
    job: PredictionJob,
    task_queue: VecDeque<QueryTask>,
    known_bkh: HashMap<String, u64>,  // name → exam_number (from known bkh table)
    batch_counter: u32,
}

impl TaskScheduler {
    fn new(job: PredictionJob, known_bkh: HashMap<String, u64>) -> Self {
        Self {
            job,
            task_queue: VecDeque::new(),
            known_bkh,
            batch_counter: 0,
        }
    }

    /// 生成下一批任务，返回 None 表示所有阶段完成
    fn generate_batch(&mut self) -> Option<TaskBatch> {
        // 如果队列里还有任务，先消耗
        if !self.task_queue.is_empty() {
            return self.pop_batch();
        }

        // 根据当前阶段生成新任务
        match self.job.phase {
            ScanPhase::SeedDiscovery => {
                self.generate_seed_tasks();
                if self.task_queue.is_empty() {
                    // 种子阶段完成，切换到班级扩展
                    self.job.phase = ScanPhase::ClassExpansion;
                    self.generate_expand_tasks();
                }
            }
            ScanPhase::ClassExpansion => {
                self.generate_expand_tasks();
                if self.task_queue.is_empty() {
                    // 扩展完成，切换到残留清扫
                    self.job.phase = ScanPhase::Cleanup;
                    self.generate_cleanup_tasks();
                }
            }
            ScanPhase::Cleanup => {
                self.generate_cleanup_tasks();
                if self.task_queue.is_empty() {
                    self.job.phase = ScanPhase::Completed;
                    return None;
                }
            }
            ScanPhase::Completed => return None,
        }

        self.pop_batch()
    }

    fn pop_batch(&mut self) -> Option<TaskBatch> {
        if self.task_queue.is_empty() {
            return None;
        }
        let mut tasks = Vec::new();
        while tasks.len() < BATCH_SIZE && !self.task_queue.is_empty() {
            if let Some(task) = self.task_queue.pop_front() {
                tasks.push(task);
            }
        }
        if tasks.is_empty() {
            return None;
        }
        let batch_id = self.batch_counter;
        self.batch_counter += 1;
        Some(TaskBatch { tasks, batch_id })
    }

    /// 阶段1: 种子发现
    /// - 如果有已知报考号表，直接用
    /// - 否则，对每个班级选代表学生，在范围内稀疏探测
    fn generate_seed_tasks(&mut self) {
        // 先用已知报考号表做种子（0查询成本）
        if !self.known_bkh.is_empty() {
            // 已知表里的人直接加入匹配（不需要查询）
            let known_pairs: Vec<(String, String, u64, u32)> = self.job.unmatched_students.iter()
                .filter_map(|s| {
                    if let Some(&exam_num) = self.known_bkh.get(&s.name) {
                        Some((s.name.clone(), s.sfz.clone(), exam_num, s.class_num))
                    } else {
                        None
                    }
                })
                .collect();

            for (name, sfz, exam_num, class_num) in &known_pairs {
                self.job.record_match(name, sfz, *exam_num, *class_num);
            }
            // 种子直接来自已知表，无需查询
            self.task_queue.clear();
            return;
        }

        // 无已知表：稀疏探测
        let unanchored = self.job.unanchored_classes();
        if unanchored.is_empty() {
            return;
        }

        let scan_low = self.job.start_bkh;
        let scan_high = self.job.end_bkh;
        let range = scan_high - scan_low;

        // 对每个无锚点的班级，选一个代表学生
        for class_num in &unanchored {
            let rep = self.job.unmatched_of_class(*class_num).first().cloned();
            if let Some(rep) = rep {
                // 均匀探测：从范围中取若干点
                let num_probes = (range / 50).min(40).max(10) as usize;
                for i in 0..num_probes {
                    let offset = (range as usize * i / num_probes) as u64;
                    let probe_number = scan_low + offset;
                    if !self.job.scanned_numbers.contains(&probe_number) {
                        self.task_queue.push_back(QueryTask {
                            exam_number: probe_number,
                            student_sfz: rep.sfz.clone(),
                            student_name: rep.name.clone(),
                            class_num: *class_num,
                            task_type: TaskType::SeedProbe,
                        });
                    }
                }
            }
        }
    }

    /// 阶段2: 班级扩展
    /// - 对已有锚点的班级，向两侧扩展
    /// - 对无锚点的班级，选代表学生在已知区域附近探测
    fn generate_expand_tasks(&mut self) {
        let anchored = self.job.anchored_classes();

        for class_num in &anchored {
            self.generate_expand_for_class(*class_num);
        }

        // 对无锚点班级，在已知区域间隙中探测
        let unanchored = self.job.unanchored_classes();
        for class_num in &unanchored {
            self.generate_class_probe(*class_num);
        }
    }

    /// 为单个班级生成扩展任务
    fn generate_expand_for_class(&mut self, class_num: u32) {
        let anchors: Vec<&Anchor> = self.job.anchors.iter()
            .filter(|a| a.class_num == class_num)
            .collect();

        if anchors.is_empty() {
            return;
        }

        let unmatched_class: Vec<&StudentInfo> = self.job.unmatched_of_class(class_num);
        if unmatched_class.is_empty() {
            return;
        }

        // 找到该班级区域的当前边界
        let zone = self.job.class_zones.iter()
            .find(|z| z.class_num == class_num);

        let (left_bound, right_bound) = if let Some(z) = zone {
            (z.start_number, z.end_number)
        } else {
            // 没有zone，用锚点的最小/最大号码
            let min = anchors.iter().map(|a| a.exam_number).min().unwrap_or(0);
            let max = anchors.iter().map(|a| a.exam_number).max().unwrap_or(0);
            (min, max)
        };

        // 向左扩展 (left_bound - 1, left_bound - 2, ...)
        let left_start = if left_bound > self.job.start_bkh { left_bound - 1 } else { self.job.start_bkh };
        for offset in 1..=EXPAND_MAX_TRIES_PER_NUMBER as u64 {
            let num = if left_start >= offset { left_start - offset + 1 } else { break };
            // 不超过班级预期大小（约50人）的距离
            if offset > 60 { break; }
            if self.job.scanned_numbers.contains(&num) { continue; }
            if num < self.job.start_bkh { break; }

            // 为这个号码尝试同班未匹配学生（最多EXPAND_MAX_TRIES_PER_NUMBER个）
            for student in unmatched_class.iter().take(EXPAND_MAX_TRIES_PER_NUMBER) {
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: student.sfz.clone(),
                    student_name: student.name.clone(),
                    class_num,
                    task_type: TaskType::ClassExpand,
                });
            }
            break; // 每次只扩展1个号码，等结果后再继续
        }

        // 向右扩展 (right_bound + 1, right_bound + 2, ...)
        let right_start = if right_bound < self.job.end_bkh { right_bound + 1 } else { self.job.end_bkh };
        for offset in 0..EXPAND_MAX_TRIES_PER_NUMBER as u64 {
            let num = right_start + offset;
            if offset > 60 { break; }
            if self.job.scanned_numbers.contains(&num) { continue; }
            if num > self.job.end_bkh { break; }

            for student in unmatched_class.iter().take(EXPAND_MAX_TRIES_PER_NUMBER) {
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: student.sfz.clone(),
                    student_name: student.name.clone(),
                    class_num,
                    task_type: TaskType::ClassExpand,
                });
            }
            break;
        }
    }

    /// 为无锚点班级生成探测任务
    fn generate_class_probe(&mut self, class_num: u32) {
        let rep = self.job.unmatched_of_class(class_num).first().cloned();
        if let Some(rep) = rep {
            // 在已知班级区域的间隙中搜索
            let mut candidate_numbers: Vec<u64> = Vec::new();

            // 在已知区域之间找间隙
            let mut zone_boundaries: Vec<u64> = self.job.class_zones.iter()
                .flat_map(|z| [z.start_number, z.end_number])
                .collect();
            zone_boundaries.sort();

            if zone_boundaries.is_empty() {
                // 没有任何已知区域，从范围中间开始探测
                let mid = (self.job.start_bkh + self.job.end_bkh) / 2;
                for offset in 0..CLASS_PROBE_RANGE {
                    let num = mid + offset;
                    if num <= self.job.end_bkh && !self.job.scanned_numbers.contains(&num) {
                        candidate_numbers.push(num);
                    }
                    if offset > 0 {
                        let num = mid - offset;
                        if num >= self.job.start_bkh && !self.job.scanned_numbers.contains(&num) {
                            candidate_numbers.push(num);
                        }
                    }
                }
            } else {
                // 在区域间隙中搜索
                for i in 0..zone_boundaries.len().saturating_sub(1) {
                    let gap_start = zone_boundaries[i] + 1;
                    let gap_end = zone_boundaries[i + 1].saturating_sub(1);
                    if gap_start >= gap_end { continue; }

                    // 在间隙中均匀取点
                    let gap_size = gap_end - gap_start;
                    let step = (gap_size / 10).max(1);
                    let mut num = gap_start;
                    while num <= gap_end {
                        if !self.job.scanned_numbers.contains(&num) {
                            candidate_numbers.push(num);
                        }
                        num += step;
                    }
                }

                // 也在范围两端搜索
                for offset in 0..CLASS_PROBE_RANGE.min(100) {
                    let num = self.job.start_bkh + offset;
                    if num <= self.job.end_bkh && !self.job.scanned_numbers.contains(&num) {
                        candidate_numbers.push(num);
                    }
                    let num = self.job.end_bkh - offset;
                    if num >= self.job.start_bkh && !self.job.scanned_numbers.contains(&num) {
                        candidate_numbers.push(num);
                    }
                }
            }

            candidate_numbers.sort();
            candidate_numbers.dedup();

            for num in candidate_numbers.into_iter().take(200) {
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: rep.sfz.clone(),
                    student_name: rep.name.clone(),
                    class_num,
                    task_type: TaskType::ClassProbe,
                });
            }
        }
    }

    /// 阶段3: 残留清扫
    fn generate_cleanup_tasks(&mut self) {
        let unmatched = self.job.unmatched_students.clone();
        if unmatched.is_empty() {
            return;
        }

        // 对每个未匹配学生，在所有未被标记为已匹配的号码上尝试
        let matched_numbers: HashSet<u64> = self.job.matched_pairs.iter()
            .map(|p| p.exam_number)
            .collect();

        // 优先在已知班级区域的边界附近搜索
        let mut candidate_numbers: Vec<u64> = Vec::new();

        // 班级区域边界附近 ±5
        for zone in &self.job.class_zones {
            for offset in 1..=5u64 {
                let left = zone.start_number.saturating_sub(offset);
                let right = zone.end_number + offset;
                if left >= self.job.start_bkh && !matched_numbers.contains(&left) {
                    candidate_numbers.push(left);
                }
                if right <= self.job.end_bkh && !matched_numbers.contains(&right) {
                    candidate_numbers.push(right);
                }
            }
        }

        // 然后在整个范围内均匀搜索
        let range = self.job.end_bkh - self.job.start_bkh;
        let step = (range / 200).max(1);
        let mut num = self.job.start_bkh;
        while num <= self.job.end_bkh {
            if !matched_numbers.contains(&num) {
                candidate_numbers.push(num);
            }
            num += step;
        }

        candidate_numbers.sort();
        candidate_numbers.dedup();

        // 每个未匹配学生 × 候选号码
        for student in &unmatched {
            for &num in candidate_numbers.iter().take(300) {
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: student.sfz.clone(),
                    student_name: student.name.clone(),
                    class_num: student.class_num,
                    task_type: TaskType::Cleanup,
                });
            }
        }
    }

    /// 处理任务结果
    fn process_results(&mut self, results: &[TaskResult]) {
        for result in results {
            self.job.scanned_numbers.insert(result.exam_number);
            self.job.total_queries += 1;

            if result.matched {
                self.job.record_match(
                    &result.student_name,
                    &result.student_sfz,
                    result.exam_number,
                    result.class_num,
                );
            }
        }

        // 检查阶段切换
        if self.job.phase == ScanPhase::SeedDiscovery && !self.job.anchors.is_empty() {
            self.job.phase = ScanPhase::ClassExpansion;
        }

        // 更新班级区域的总人数
        let class_counts: HashMap<u32, usize> = {
            let mut counts = HashMap::new();
            for s in &self.job.unmatched_students {
                *counts.entry(s.class_num).or_insert(0) += 1;
            }
            for p in &self.job.matched_pairs {
                *counts.entry(p.class_num).or_insert(0) += 1;
            }
            counts
        };
        for zone in &mut self.job.class_zones {
            zone.total_count = *class_counts.get(&zone.class_num).unwrap_or(&0);
        }

        // 如果所有学生都匹配了
        if self.job.unmatched_students.is_empty() {
            self.job.phase = ScanPhase::Completed;
        }
    }
}

// ═══════════════════════════════════════════════════════════
//  主入口：运行推算
// ═══════════════════════════════════════════════════════════

pub async fn run_prediction(
    pool: Arc<BrowserPool>,
    job: PredictionJob,
    known_bkh: HashMap<String, u64>,
    concurrency: usize,
    cancel_flag: Arc<Mutex<bool>>,
    progress: Arc<Mutex<PredictionProgress>>,
    logs: Arc<Mutex<Vec<String>>>,
    perf_logs: Arc<Mutex<Vec<Vec<PerfEvent>>>>,
    captcha_stats: Arc<Mutex<CaptchaStats>>,
    browser_statuses: Arc<Mutex<Vec<BrowserStatus>>>,
) -> Vec<PredictedRecord> {
    let total_students = job.total_students;
    let _job_id = job.id.clone();
    let job_name = job.name.clone();

    // 初始化进度
    {
        let mut p = progress.lock().await;
        p.total = total_students;
        p.matched = job.matched_count;
        p.not_found = 0;
        p.phase = job.phase.label().to_string();
    }

    {
        let mut l = logs.lock().await;
        l.push(format!("🚀 [班级锚点扩展] 启动！任务={} | 学生数={} | 已匹配={} | 阶段={}",
            job_name, total_students, job.matched_count, job.phase.label()));
    }

    // 初始化调度器
    let scheduler = Arc::new(Mutex::new(TaskScheduler::new(job, known_bkh)));

    // 启动工人
    let mut worker_handles = Vec::new();

    for worker_id in 0..concurrency {
        let pool = pool.clone();
        let scheduler = scheduler.clone();
        let cancel_flag = cancel_flag.clone();
        let progress = progress.clone();
        let logs = logs.clone();
        let perf_logs = perf_logs.clone();
        let captcha_stats = captcha_stats.clone();
        let browser_statuses = browser_statuses.clone();

        worker_handles.push(tokio::spawn(async move {
            loop {
                if *cancel_flag.lock().await { break; }

                // 从调度器获取一批任务
                let batch = {
                    let mut sched = scheduler.lock().await;
                    sched.generate_batch()
                };

                let batch = match batch {
                    Some(b) => b,
                    None => break, // 所有任务完成
                };

                // 获取浏览器
                let (permit, mut client) = pool.acquire().await;
                client.set_captcha_stats(Some(captcha_stats.clone()));
                client.set_status(Some(browser_statuses.clone()));
                client.set_turbo(true);
                let record_perf = Arc::new(Mutex::new(Vec::new()));
                client.set_perf(Some(record_perf.clone()));

                // 执行批次中的所有任务
                let mut batch_results = Vec::new();
                for task in &batch.tasks {
                    if *cancel_flag.lock().await { break; }

                    let full_exam_number = task.exam_number.to_string();

                    // 更新进度
                    {
                        let mut p = progress.lock().await;
                        p.current_name = task.student_name.clone();
                        p.current_exam = full_exam_number.clone();
                        p.current_batch = format!(
                            "[{}] 考号 {} ← {} ({}班)",
                            task.task_type_label(),
                            task.exam_number,
                            task.student_name,
                            task.class_num,
                        );
                    }

                    // 执行查询
                    let result = client.query_single(
                        &full_exam_number,
                        &task.student_sfz,
                        &task.student_name,
                    ).await;

                    // go_home 在 pool.release() 中自动调用，无需手动

                    let matched = match &result {
                        Ok(res) => {
                            // 检查返回的姓名是否匹配
                            res.name == task.student_name
                        }
                        Err(_) => false,
                    };

                    let error = match &result {
                        Err(e) => e.clone(),
                        _ => String::new(),
                    };

                    batch_results.push(TaskResult {
                        exam_number: task.exam_number,
                        student_sfz: task.student_sfz.clone(),
                        student_name: task.student_name.clone(),
                        class_num: task.class_num,
                        task_type: task.task_type.clone(),
                        matched,
                        error,
                    });

                    if matched {
                        let mut l = logs.lock().await;
                        l.push(format!(
                            "✅ [命中] 工人#{} 考号 {} 命中！学生：{} ({}班)",
                            worker_id, task.exam_number, task.student_name, task.class_num
                        ));
                    }
                }

                // 收集性能数据
                if let Ok(perf_data) = record_perf.try_lock() {
                    if !perf_data.is_empty() {
                        let mut pl = perf_logs.lock().await;
                        pl.push(perf_data.clone());
                    }
                }

                pool.release(permit, client);

                // 将结果返回给调度器处理
                {
                    let mut sched = scheduler.lock().await;
                    sched.process_results(&batch_results);

                    // 更新进度
                    let matched_count = sched.job.matched_count;
                    let unmatched_count = sched.job.unmatched_students.len();
                    let total_queries = sched.job.total_queries;
                    let phase = sched.job.phase.label().to_string();

                    {
                        let mut p = progress.lock().await;
                        p.matched = matched_count;
                        p.total_queries = total_queries;
                        p.phase = phase;
                    }

                    // 持久化任务（每批处理完保存一次）
                    if let Err(e) = crate::job::save_job(&sched.job) {
                        let mut l = logs.lock().await;
                        l.push(format!("⚠️ 保存任务进度失败: {}", e));
                    }
                }
            }
        }));
    }

    // 等待所有工人完成
    for h in worker_handles { let _ = h.await; }

    // 收集最终结果
    let final_job = {
        let sched = scheduler.lock().await;
        sched.job.clone()
    };

    // 最终持久化
    let _ = crate::job::save_job(&final_job);

    // 生成输出记录
    let mut out_records = Vec::new();

    for pair in &final_job.matched_pairs {
        out_records.push(PredictedRecord {
            name: pair.name.clone(),
            shenfenzheng: pair.sfz.clone(),
            exam_number: pair.exam_number.to_string(),
            status: PredictedStatus::Matched,
        });
    }

    for student in &final_job.unmatched_students {
        out_records.push(PredictedRecord {
            name: student.name.clone(),
            shenfenzheng: student.sfz.clone(),
            exam_number: "扫描范围外".to_string(),
            status: PredictedStatus::NotFound,
        });
    }

    // 更新进度
    {
        let mut p = progress.lock().await;
        p.matched = final_job.matched_count;
        p.not_found = final_job.unmatched_students.len();
    }

    {
        let mut l = logs.lock().await;
        l.push(format!(
            "🏁 [推算完成] 最终结果：命中 {} / {}，未找到 {}，总查询 {} 次",
            final_job.matched_count, total_students,
            final_job.unmatched_students.len(),
            final_job.total_queries
        ));
    }

    out_records
}

// ═══════════════════════════════════════════════════════════
//  QueryTask 辅助方法
// ═══════════════════════════════════════════════════════════

impl QueryTask {
    fn task_type_label(&self) -> &str {
        match self.task_type {
            TaskType::SeedProbe => "种子探测",
            TaskType::ClassExpand => "班级扩展",
            TaskType::ClassProbe => "班级探测",
            TaskType::Cleanup => "残留清扫",
        }
    }
}
