use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
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

/// 种子扫描时，每个号码尝试几个班级代表（轮转）
const SEED_REPS_PER_NUMBER: usize = 3;

/// 一次生成任务的上限（防止队列过长）
const MAX_TASKS_PER_GENERATION: usize = BATCH_SIZE * 5;

// ═══════════════════════════════════════════════════════════
//  任务调度器：号码中心扫描 + 班级代表轮转
// ═══════════════════════════════════════════════════════════

struct TaskScheduler {
    job: PredictionJob,
    task_queue: VecDeque<QueryTask>,
    known_bkh: HashMap<String, u64>,  // name → exam_number (from known bkh table)
    batch_counter: u32,
    /// 种子扫描游标：从 end_bkh 向 start_bkh 递减扫描
    seed_cursor: u64,
    /// 班级代表轮转索引
    rep_rotation: usize,
    /// 是否已处理过已知报考号
    known_bkh_processed: bool,
}

impl TaskScheduler {
    fn new(job: PredictionJob, known_bkh: HashMap<String, u64>) -> Self {
        // seed_cursor 从 job 中恢复（已持久化），新任务时为 end_bkh
        let seed_cursor = job.seed_cursor;
        // 如果任务已有匹配记录，说明 known_bkh 之前已处理过
        let known_bkh_processed = !job.matched_pairs.is_empty() || known_bkh.is_empty();
        Self {
            job,
            task_queue: VecDeque::new(),
            known_bkh,
            batch_counter: 0,
            seed_cursor,
            rep_rotation: 0,
            known_bkh_processed,
        }
    }

    /// 生成下一批任务，返回 None 表示所有阶段完成
    fn generate_batch(&mut self, cancelled: bool) -> Option<TaskBatch> {
        // 如果已取消，不再生成新任务，只消耗队列中已有的
        if cancelled {
            if self.task_queue.is_empty() {
                return None;
            }
            return self.pop_batch();
        }

        // 如果队列里还有任务，先消耗
        if !self.task_queue.is_empty() {
            return self.pop_batch();
        }

        // 根据当前阶段生成新任务
        match self.job.phase {
            ScanPhase::SeedDiscovery => {
                self.generate_seed_tasks();
                if self.task_queue.is_empty() {
                    // 种子阶段完成（所有号码扫完或所有班级都有锚点），切换到班级扩展
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

    // ═══════════════════════════════════════════════════════════
    //  阶段1: 种子发现（号码中心扫描）
    //
    //  核心思路：从 end_bkh 向 start_bkh 逐号扫描
    //  每个号码尝试几个不同班级的代表（轮转），而非一个人试很多号
    //  这样每批10个任务里会有不同班级的学生，不会"一直拿同一个人撞"
    // ═══════════════════════════════════════════════════════════

    fn generate_seed_tasks(&mut self) {
        // 先用已知报考号表做种子（0查询成本）
        if !self.known_bkh_processed && !self.known_bkh.is_empty() {
            self.known_bkh_processed = true;
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
            self.task_queue.clear();
            return;
        }

        // 获取所有无锚点的班级
        let unanchored = self.job.unanchored_classes();
        if unanchored.is_empty() {
            // 所有班级都找到了锚点，种子阶段完成
            return;
        }

        // 为每个无锚点班级选一个代表学生
        let reps: Vec<(u32, StudentInfo)> = unanchored.iter()
            .filter_map(|&c| {
                self.job.unmatched_of_class(c).first().cloned().map(|s| (c, s.clone()))
            })
            .collect();

        if reps.is_empty() {
            return;
        }

        // 号码中心扫描：从 seed_cursor 向下扫描
        let mut tasks_generated = 0;

        while self.seed_cursor >= self.job.start_bkh && tasks_generated < MAX_TASKS_PER_GENERATION {
            let num = self.seed_cursor;

            // 向下移动游标（不管当前号码是否跳过，游标都要前进）
            if self.seed_cursor == 0 {
                break;
            }
            self.seed_cursor -= 1;

            // 跳过已扫描的号码
            if self.job.scanned_numbers.contains(&num) {
                continue;
            }

            // 轮转选取班级代表：每次取 SEED_REPS_PER_NUMBER 个代表
            let reps_count = reps.len();
            for i in 0..SEED_REPS_PER_NUMBER.min(reps_count) {
                let idx = (self.rep_rotation + i) % reps_count;
                let (class_num, rep) = &reps[idx];
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: rep.sfz.clone(),
                    student_name: rep.name.clone(),
                    class_num: *class_num,
                    task_type: TaskType::SeedProbe,
                });
                tasks_generated += 1;
            }

            // 轮转偏移，让下一轮号码尝试不同的班级组合
            self.rep_rotation = (self.rep_rotation + 1) % reps_count;
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段2: 班级扩展
    //
    //  对已有锚点的班级：从锚点向两侧扩展，每边每次只扩展1个号码
    //  对每个边界号码，只尝试1个未匹配学生（轮流），而非一次试50个
    //  对无锚点班级：继续用号码中心扫描（与种子阶段相同策略）
    // ═══════════════════════════════════════════════════════════

    fn generate_expand_tasks(&mut self) {
        let anchored = self.job.anchored_classes();
        let unanchored = self.job.unanchored_classes();

        // 对有锚点的班级，向两侧扩展
        for class_num in &anchored {
            self.generate_expand_for_class(*class_num);
        }

        // 对无锚点的班级，继续号码中心扫描（复用种子扫描的游标）
        if !unanchored.is_empty() {
            self.generate_seed_tasks();
        }
    }

    /// 为单个有锚点班级生成扩展任务
    /// 每个边界号码只尝试1个未匹配学生，轮流使用不同学生
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
            let min = anchors.iter().map(|a| a.exam_number).min().unwrap_or(0);
            let max = anchors.iter().map(|a| a.exam_number).max().unwrap_or(0);
            (min, max)
        };

        // 向左扩展：找到下一个未扫描的号码，只试1个学生
        if left_bound > self.job.start_bkh {
            for offset in 1..=60u64 {
                let num = if left_bound >= offset { left_bound - offset } else { break };
                if num < self.job.start_bkh { break; }
                if self.job.scanned_numbers.contains(&num) { continue; }

                // 只选1个未匹配学生（轮流选取）
                let student_idx = (num as usize) % unmatched_class.len();
                let student = &unmatched_class[student_idx];
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: student.sfz.clone(),
                    student_name: student.name.clone(),
                    class_num,
                    task_type: TaskType::ClassExpand,
                });
                break; // 每次只扩展1个号码
            }
        }

        // 向右扩展：同理
        if right_bound < self.job.end_bkh {
            for offset in 1..=60u64 {
                let num = right_bound + offset;
                if num > self.job.end_bkh { break; }
                if self.job.scanned_numbers.contains(&num) { continue; }

                let student_idx = (num as usize) % unmatched_class.len();
                let student = &unmatched_class[student_idx];
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: student.sfz.clone(),
                    student_name: student.name.clone(),
                    class_num,
                    task_type: TaskType::ClassExpand,
                });
                break; // 每次只扩展1个号码
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段3: 残留清扫
    //
    //  对剩余未匹配学生，在班级区域边界和范围中搜索
    //  每个号码只尝试1个学生（交错分配），避免一个人撞所有号
    // ═══════════════════════════════════════════════════════════

    fn generate_cleanup_tasks(&mut self) {
        let unmatched = self.job.unmatched_students.clone();
        if unmatched.is_empty() {
            return;
        }

        let matched_numbers: HashSet<u64> = self.job.matched_pairs.iter()
            .map(|p| p.exam_number)
            .collect();

        // 收集候选号码
        let mut candidate_numbers: Vec<u64> = Vec::new();

        // 优先在班级区域边界附近搜索 ±10
        for zone in &self.job.class_zones {
            for offset in 1..=10u64 {
                let left = zone.start_number.saturating_sub(offset);
                let right = zone.end_number + offset;
                if left >= self.job.start_bkh && !matched_numbers.contains(&left) && !self.job.scanned_numbers.contains(&left) {
                    candidate_numbers.push(left);
                }
                if right <= self.job.end_bkh && !matched_numbers.contains(&right) && !self.job.scanned_numbers.contains(&right) {
                    candidate_numbers.push(right);
                }
            }
        }

        // 然后在未扫描的号码中均匀搜索
        let range = self.job.end_bkh - self.job.start_bkh;
        let step = (range / 200).max(1);
        let mut num = self.job.start_bkh;
        while num <= self.job.end_bkh {
            if !matched_numbers.contains(&num) && !self.job.scanned_numbers.contains(&num) {
                candidate_numbers.push(num);
            }
            num += step;
        }

        candidate_numbers.sort();
        candidate_numbers.dedup();

        // 交错分配：每个号码分配给不同的学生
        // 而非每个学生试所有号码（那样会导致同一个人反复出现）
        let nums_to_try = candidate_numbers.into_iter().take(500).collect::<Vec<_>>();
        let student_count = unmatched.len();

        for (i, &num) in nums_to_try.iter().enumerate() {
            // 交错：第i个号码分配给第(i % student_count)个学生
            let student = &unmatched[i % student_count];
            self.task_queue.push_back(QueryTask {
                exam_number: num,
                student_sfz: student.sfz.clone(),
                student_name: student.name.clone(),
                class_num: student.class_num,
                task_type: TaskType::Cleanup,
            });
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

        // 注意：不再在 process_results 中自动切换阶段！
        // 阶段切换只在 generate_batch 中进行，当当前阶段的任务队列为空时才推进。
        // 这样可以避免用户停止推算时，process_results 发现锚点就跳阶段的问题。

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
    cancel_flag: Arc<AtomicBool>,
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
        l.push(format!("🚀 [号码中心扫描] 启动！任务={} | 学生数={} | 已匹配={} | 阶段={}",
            job_name, total_students, job.matched_count, job.phase.label()));
        l.push(format!("   策略：逐号扫描 + 班级代表轮转，每号尝试{}个不同班级代表", SEED_REPS_PER_NUMBER));
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
                if cancel_flag.load(AtomicOrdering::Relaxed) { break; }

                // 检查浏览器池是否已关闭（用户点击了停止）
                if pool.is_shutdown() { break; }

                // 从调度器获取一批任务（传入取消状态，避免取消后阶段跳转）
                let is_cancelled = cancel_flag.load(AtomicOrdering::Relaxed);
                let batch = {
                    let mut sched = scheduler.lock().await;
                    sched.generate_batch(is_cancelled)
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
                    if cancel_flag.load(AtomicOrdering::Relaxed) { break; }

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
                    let total_queries = sched.job.total_queries;
                    let phase = sched.job.phase.label().to_string();
                    let cursor = sched.seed_cursor;
                    let scanned = sched.job.scanned_numbers.len();
                    let total_students = sched.job.total_students;

                    {
                        let mut p = progress.lock().await;
                        p.matched = matched_count;
                        p.total_queries = total_queries;
                        p.phase = phase;
                        p.not_found = scanned - matched_count;
                    }

                    // 同步 seed_cursor 到 job（持久化时需要保存）
                    sched.job.seed_cursor = sched.seed_cursor;

                    // 持久化任务（每批处理完保存一次）
                    let save_err = crate::job::save_job(&sched.job).err();

                    // 合并所有日志写入到一个锁中
                    {
                        let mut l = logs.lock().await;
                        if let Some(e) = save_err {
                            l.push(format!("⚠️ 保存任务进度失败: {}", e));
                        }
                        l.push(format!(
                            "📊 游标={} | 扫描={} | 匹配={}/{} | 查询={}",
                            cursor, scanned, matched_count, total_students, total_queries
                        ));
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
            TaskType::SeedProbe => "种子扫描",
            TaskType::ClassExpand => "班级扩展",
            TaskType::ClassProbe => "班级探测",
            TaskType::Cleanup => "残留清扫",
        }
    }
}
