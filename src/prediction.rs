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

/// 种子号码间距（每20号插一个种子）
const SEED_SPACING: u64 = 20;

/// 种子号码数量（100号范围内取5个种子）
const SEED_COUNT: usize = 5;

/// 种子搜索范围（从end_bkh往前搜索100号）
const SEED_RANGE: u64 = 100;

/// 班级扩展步长（从锚点向两侧扩展时的搜索范围）
const EXPAND_SEARCH_RADIUS: u64 = 60;

/// 一次生成任务的上限（防止队列过长）
const MAX_TASKS_PER_GENERATION: usize = BATCH_SIZE * 10;

// ═══════════════════════════════════════════════════════════
//  任务调度器：两班递进扫描算法
//
//  核心思路：
//  1. 将班级按编号排序，每次取2个班
//  2. 从end_bkh往前100号，每20号插1个种子，共5个种子
//  3. 用这2个班的所有学生撞这5个种子号码
//  4. 找到锚点后，扩展搜索该班级区域
//  5. 完成后，用最小报考号作为新的end_bkh，继续前两个班
// ═══════════════════════════════════════════════════════════

struct TaskScheduler {
    job: PredictionJob,
    task_queue: VecDeque<QueryTask>,
    known_bkh: HashMap<String, u64>,
    batch_counter: u32,

    // 两班递进扫描状态
    /// 所有班级号，从大到小排列
    all_classes: Vec<u32>,
    /// 当前处理的2个班级号
    current_pair: Vec<u32>,
    /// 当前对的种子号码列表
    current_seeds: Vec<u64>,
    /// 当前对中已被种子命中的号码集合
    seed_hits: HashSet<u64>,
    /// 班级扩展游标：class_num → (left_offset, right_offset)
    expand_cursors: HashMap<u32, u64>,
}

impl TaskScheduler {
    fn new(job: PredictionJob, known_bkh: HashMap<String, u64>) -> Self {
        // 获取所有班级号，从大到小排列
        let mut all_classes: Vec<u32> = job.unmatched_students.iter()
            .map(|s| s.class_num)
            .filter(|&c| c > 0)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        all_classes.sort_by(|a, b| b.cmp(a)); // 降序：17, 16, 15, ...

        // 恢复时跳过已完成的班级
        let pair_idx = job.class_pair_idx;
        let current_pair: Vec<u32> = all_classes.iter()
            .skip(pair_idx * 2)
            .take(2)
            .copied()
            .collect();

        // 计算当前对的种子号码
        let pair_cursor = job.pair_cursor;
        let current_seeds = Self::calc_seed_numbers(pair_cursor, job.start_bkh);

        let mut scheduler = Self {
            job,
            task_queue: VecDeque::new(),
            known_bkh,
            batch_counter: 0,
            all_classes,
            current_pair,
            current_seeds,
            seed_hits: HashSet::new(),
            expand_cursors: HashMap::new(),
        };

        // 恢复 seed_hits（从已有锚点中恢复）
        scheduler.recover_seed_hits();

        scheduler
    }

    /// 计算从 pair_cursor 往前的种子号码
    fn calc_seed_numbers(cursor: u64, start_bkh: u64) -> Vec<u64> {
        let mut seeds = Vec::new();
        for i in 0..SEED_COUNT {
            let num = cursor.saturating_sub(i as u64 * SEED_SPACING);
            if num < start_bkh { break; }
            seeds.push(num);
        }
        seeds
    }

    /// 从已有锚点恢复 seed_hits
    fn recover_seed_hits(&mut self) {
        for seed_num in &self.current_seeds {
            if self.job.anchors.iter().any(|a| a.exam_number == *seed_num) {
                self.seed_hits.insert(*seed_num);
            }
        }
        // 也从已扫描号码中恢复
        for seed_num in &self.current_seeds {
            if self.job.scanned_numbers.contains(seed_num) {
                self.seed_hits.insert(*seed_num);
            }
        }
    }

    /// 生成下一批任务，返回 None 表示所有阶段完成
    fn generate_batch(&mut self, cancelled: bool) -> Option<TaskBatch> {
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
            ScanPhase::PairSeed => {
                self.generate_pair_seed_tasks();
                if self.task_queue.is_empty() {
                    // 种子任务全部完成，推进到扩展阶段
                    self.advance_to_expand();
                    self.generate_pair_expand_tasks();
                }
            }
            ScanPhase::PairExpand => {
                self.generate_pair_expand_tasks();
                if self.task_queue.is_empty() {
                    // 扩展完成，推进到扫描阶段
                    self.advance_to_scan();
                    self.generate_pair_scan_tasks();
                }
            }
            ScanPhase::PairScan => {
                self.generate_pair_scan_tasks();
                if self.task_queue.is_empty() {
                    // 当前对扫描完成，推进到下一对
                    self.advance_to_next_pair();
                    if self.job.phase == ScanPhase::PairSeed {
                        // 新一对班级，重新生成种子任务
                        self.generate_pair_seed_tasks();
                    }
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
    //  阶段1: 两班种子 — 用2个班的所有学生撞5个种子号码
    // ═══════════════════════════════════════════════════════════

    fn generate_pair_seed_tasks(&mut self) {
        if self.current_pair.is_empty() {
            return;
        }

        // 先用已知报考号做零成本匹配
        self.apply_known_bkh();

        let mut tasks_generated = 0;

        for &seed_num in &self.current_seeds {
            // 跳过已命中的种子
            if self.seed_hits.contains(&seed_num) {
                continue;
            }
            // 跳过已扫描过的号码
            if self.job.scanned_numbers.contains(&seed_num) {
                continue;
            }

            // 用当前2个班的所有未匹配学生撞这个种子号码
            for &class_num in &self.current_pair {
                let unmatched = self.job.unmatched_of_class(class_num);
                for student in unmatched {
                    self.task_queue.push_back(QueryTask {
                        exam_number: seed_num,
                        student_sfz: student.sfz.clone(),
                        student_name: student.name.clone(),
                        class_num,
                        task_type: TaskType::SeedProbe,
                    });
                    tasks_generated += 1;
                    if tasks_generated >= MAX_TASKS_PER_GENERATION {
                        return;
                    }
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段2: 两班扩展 — 从锚点向两侧扩展搜索
    // ═══════════════════════════════════════════════════════════

    fn generate_pair_expand_tasks(&mut self) {
        let mut tasks_generated = 0;

        for &class_num in &self.current_pair {
            let unmatched = self.job.unmatched_of_class(class_num);
            if unmatched.is_empty() {
                continue; // 这个班已全部匹配
            }

            // 获取该班级的锚点
            let class_anchors: Vec<&Anchor> = self.job.anchors.iter()
                .filter(|a| a.class_num == class_num)
                .collect();

            if class_anchors.is_empty() {
                continue; // 没有锚点，无法扩展（种子阶段未命中）
            }

            // 获取班级区域
            let zone = self.job.class_zones.iter()
                .find(|z| z.class_num == class_num);

            let (left_bound, right_bound) = if let Some(z) = zone {
                (z.start_number, z.end_number)
            } else {
                let min = class_anchors.iter().map(|a| a.exam_number).min().unwrap_or(0);
                let max = class_anchors.iter().map(|a| a.exam_number).max().unwrap_or(0);
                (min, max)
            };

            // 获取扩展偏移
            let offset = self.expand_cursors.entry(class_num).or_insert(1);

            // 向左扩展
            if left_bound > self.job.start_bkh {
                for delta in *offset..=*offset + 5 {
                    let num = if left_bound >= delta { left_bound - delta } else { break };
                    if num < self.job.start_bkh { break; }
                    if self.job.scanned_numbers.contains(&num) { continue; }
                    if self.job.matched_pairs.iter().any(|p| p.exam_number == num) { continue; }

                    // 轮流选一个未匹配学生
                    let student_idx = (num as usize) % unmatched.len();
                    let student = &unmatched[student_idx];
                    self.task_queue.push_back(QueryTask {
                        exam_number: num,
                        student_sfz: student.sfz.clone(),
                        student_name: student.name.clone(),
                        class_num,
                        task_type: TaskType::ClassExpand,
                    });
                    tasks_generated += 1;
                }
            }

            // 向右扩展
            if right_bound < self.job.end_bkh {
                for delta in *offset..=*offset + 5 {
                    let num = right_bound + delta;
                    if num > self.job.end_bkh { break; }
                    if self.job.scanned_numbers.contains(&num) { continue; }
                    if self.job.matched_pairs.iter().any(|p| p.exam_number == num) { continue; }

                    let student_idx = (num as usize) % unmatched.len();
                    let student = &unmatched[student_idx];
                    self.task_queue.push_back(QueryTask {
                        exam_number: num,
                        student_sfz: student.sfz.clone(),
                        student_name: student.name.clone(),
                        class_num,
                        task_type: TaskType::ClassExpand,
                    });
                    tasks_generated += 1;
                }
            }

            // 推进偏移
            *offset += 6;

            if tasks_generated >= MAX_TASKS_PER_GENERATION {
                return;
            }
        }

        // 如果两班都没有锚点（种子阶段一个都没命中），用大范围探测
        if tasks_generated == 0 {
            for &class_num in &self.current_pair {
                let unmatched = self.job.unmatched_of_class(class_num);
                if unmatched.is_empty() { continue; }

                // 从种子区域往前继续搜索
                let search_start = self.current_seeds.last().copied().unwrap_or(self.job.pair_cursor);
                let search_end = self.job.pair_cursor;

                for num in (search_start..=search_end).rev() {
                    if num < self.job.start_bkh { break; }
                    if self.job.scanned_numbers.contains(&num) { continue; }

                    // 每个号码只用1个学生试探
                    let student_idx = (num as usize) % unmatched.len();
                    let student = &unmatched[student_idx];
                    self.task_queue.push_back(QueryTask {
                        exam_number: num,
                        student_sfz: student.sfz.clone(),
                        student_name: student.name.clone(),
                        class_num,
                        task_type: TaskType::ClassExpand,
                    });
                    tasks_generated += 1;
                    if tasks_generated >= MAX_TASKS_PER_GENERATION {
                        return;
                    }
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段3: 两班扫描 — 在确认的班级区域内扫描剩余学生
    // ═══════════════════════════════════════════════════════════

    fn generate_pair_scan_tasks(&mut self) {
        let mut tasks_generated = 0;

        for &class_num in &self.current_pair {
            let unmatched = self.job.unmatched_of_class(class_num);
            if unmatched.is_empty() {
                continue; // 该班全部匹配
            }

            // 获取班级区域
            let zone = self.job.class_zones.iter()
                .find(|z| z.class_num == class_num);

            let (zone_start, zone_end) = if let Some(z) = zone {
                (z.start_number, z.end_number)
            } else {
                continue; // 没有区域信息，无法扫描
            };

            // 在班级区域 ±EXPAND_SEARCH_RADIUS 内，对未扫描的号码尝试未匹配学生
            let scan_start = zone_start.saturating_sub(EXPAND_SEARCH_RADIUS);
            let scan_end = std::cmp::min(zone_end + EXPAND_SEARCH_RADIUS, self.job.end_bkh);

            for num in scan_start..=scan_end {
                if num < self.job.start_bkh { continue; }
                if self.job.scanned_numbers.contains(&num) { continue; }
                if self.job.matched_pairs.iter().any(|p| p.exam_number == num) { continue; }

                // 轮流分配学生
                let student_idx = (num as usize) % unmatched.len();
                let student = &unmatched[student_idx];
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: student.sfz.clone(),
                    student_name: student.name.clone(),
                    class_num,
                    task_type: TaskType::ClassScan,
                });
                tasks_generated += 1;
                if tasks_generated >= MAX_TASKS_PER_GENERATION {
                    return;
                }
            }
        }

        // 如果当前对有班级没找到锚点，扩大搜索范围
        for &class_num in &self.current_pair {
            let unmatched = self.job.unmatched_of_class(class_num);
            if unmatched.is_empty() { continue; }

            let has_zone = self.job.class_zones.iter().any(|z| z.class_num == class_num);
            if has_zone { continue; }

            // 没有区域的班级：在种子范围 ±200 内搜索
            let search_start = self.job.pair_cursor.saturating_sub(SEED_RANGE + 100);
            let search_end = std::cmp::min(self.job.pair_cursor + 50, self.job.end_bkh);

            for num in (search_start..=search_end).rev() {
                if num < self.job.start_bkh { continue; }
                if self.job.scanned_numbers.contains(&num) { continue; }

                let student_idx = (num as usize) % unmatched.len();
                let student = &unmatched[student_idx];
                self.task_queue.push_back(QueryTask {
                    exam_number: num,
                    student_sfz: student.sfz.clone(),
                    student_name: student.name.clone(),
                    class_num,
                    task_type: TaskType::ClassScan,
                });
                tasks_generated += 1;
                if tasks_generated >= MAX_TASKS_PER_GENERATION {
                    return;
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段4: 残留清扫 — 处理最后仍未匹配的学生
    // ═══════════════════════════════════════════════════════════

    fn generate_cleanup_tasks(&mut self) {
        let unmatched = self.job.unmatched_students.clone();
        if unmatched.is_empty() {
            return;
        }

        let matched_numbers: HashSet<u64> = self.job.matched_pairs.iter()
            .map(|p| p.exam_number)
            .collect();

        let mut candidate_numbers: Vec<u64> = Vec::new();

        // 在班级区域边界附近搜索
        for zone in &self.job.class_zones {
            for offset in 1..=30u64 {
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

        // 均匀搜索
        let range = self.job.end_bkh.saturating_sub(self.job.start_bkh);
        let step = (range / 500).max(1);
        let mut num = self.job.start_bkh;
        while num <= self.job.end_bkh {
            if !matched_numbers.contains(&num) && !self.job.scanned_numbers.contains(&num) {
                candidate_numbers.push(num);
            }
            num += step;
        }

        candidate_numbers.sort();
        candidate_numbers.dedup();

        let nums_to_try = candidate_numbers.into_iter().take(500).collect::<Vec<_>>();
        let student_count = unmatched.len();

        for (i, &num) in nums_to_try.iter().enumerate() {
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

    // ═══════════════════════════════════════════════════════════
    //  阶段切换逻辑
    // ═══════════════════════════════════════════════════════════

    fn advance_to_expand(&mut self) {
        self.job.phase = ScanPhase::PairExpand;
        self.expand_cursors.clear();
        eprintln!("[推算] 阶段推进: 两班种子 → 两班扩展 (班级: {:?})", self.current_pair);
    }

    fn advance_to_scan(&mut self) {
        self.job.phase = ScanPhase::PairScan;
        eprintln!("[推算] 阶段推进: 两班扩展 → 两班扫描 (班级: {:?})", self.current_pair);
    }

    fn advance_to_next_pair(&mut self) {
        // 将当前对的班级标记为已完成
        for &class_num in &self.current_pair {
            if !self.job.completed_class_nums.contains(&class_num) {
                self.job.completed_class_nums.push(class_num);
            }
        }

        // 推进到下一对
        self.job.class_pair_idx += 1;
        let pair_idx = self.job.class_pair_idx;
        let next_pair: Vec<u32> = self.all_classes.iter()
            .skip(pair_idx * 2)
            .take(2)
            .copied()
            .collect();

        if next_pair.is_empty() {
            // 所有班级对都处理完了
            // 检查是否还有未匹配的学生
            if self.job.unmatched_students.is_empty() {
                self.job.phase = ScanPhase::Completed;
                eprintln!("[推算] 所有班级处理完成，全部匹配！");
            } else {
                self.job.phase = ScanPhase::Cleanup;
                eprintln!("[推算] 所有班级对处理完成，进入残留清扫 (剩余{}人)", self.job.unmatched_students.len());
            }
            return;
        }

        // 更新 pair_cursor：用当前对找到的最小报考号作为新起点
        let current_min_exam = self.job.matched_pairs.iter()
            .filter(|p| self.current_pair.contains(&p.class_num))
            .map(|p| p.exam_number)
            .min();

        let new_cursor = if let Some(min_num) = current_min_exam {
            // 最小号往前100号作为新搜索范围
            min_num.saturating_sub(SEED_RANGE)
        } else {
            // 当前对没找到任何人，往前推进
            self.job.pair_cursor.saturating_sub(SEED_RANGE)
        };

        self.job.pair_cursor = new_cursor.max(self.job.start_bkh);
        self.current_pair = next_pair;
        self.current_seeds = Self::calc_seed_numbers(self.job.pair_cursor, self.job.start_bkh);
        self.seed_hits.clear();
        self.expand_cursors.clear();
        self.job.phase = ScanPhase::PairSeed;
        self.recover_seed_hits();

        eprintln!("[推算] 推进到下一对班级: {:?} | cursor={} | seeds={:?}", 
            self.current_pair, self.job.pair_cursor, self.current_seeds);
    }

    /// 使用已知报考号表做零成本匹配
    fn apply_known_bkh(&mut self) {
        if self.known_bkh.is_empty() {
            return;
        }

        // 只在首次调用时使用
        if self.job.total_queries > 0 {
            return;
        }

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

                // 如果命中的是种子号码，标记种子命中
                if self.current_seeds.contains(&result.exam_number) {
                    self.seed_hits.insert(result.exam_number);
                }
            }
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
    cancel_flag: Arc<AtomicBool>,
    progress: Arc<Mutex<PredictionProgress>>,
    logs: Arc<Mutex<Vec<String>>>,
    perf_logs: Arc<Mutex<Vec<Vec<PerfEvent>>>>,
    captcha_stats: Arc<Mutex<CaptchaStats>>,
    browser_statuses: Arc<Mutex<Vec<BrowserStatus>>>,
    pred_results: Arc<Mutex<Vec<PredictedRecord>>>,
) -> Vec<PredictedRecord> {
    let total_students = job.total_students;
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
        l.push(format!("🚀 [两班递进扫描] 启动！任务={} | 学生数={} | 已匹配={} | 阶段={}",
            job_name, total_students, job.matched_count, job.phase.label()));
        l.push(format!("   策略：每次取2个班，5个种子号码，暴力撞 → 扩展 → 扫描 → 下一对"));
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
        let pred_results = pred_results.clone();

        worker_handles.push(tokio::spawn(async move {
            loop {
                if cancel_flag.load(AtomicOrdering::Relaxed) { break; }
                if pool.is_shutdown() { break; }

                let is_cancelled = cancel_flag.load(AtomicOrdering::Relaxed);
                let batch = {
                    let mut sched = scheduler.lock().await;
                    sched.generate_batch(is_cancelled)
                };

                let batch = match batch {
                    Some(b) => b,
                    None => break,
                };

                let (permit, mut client) = pool.acquire().await;
                client.set_captcha_stats(Some(captcha_stats.clone()));
                client.set_status(Some(browser_statuses.clone()));
                client.set_turbo(true);
                let record_perf = Arc::new(Mutex::new(Vec::new()));
                client.set_perf(Some(record_perf.clone()));

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

                    let result = client.query_single(
                        &full_exam_number,
                        &task.student_sfz,
                        &task.student_name,
                    ).await;

                    let matched = match &result {
                        Ok(res) => res.name == task.student_name,
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
                    let scanned = sched.job.scanned_numbers.len();
                    let total_students = sched.job.total_students;
                    let current_pair = sched.current_pair.clone();

                    {
                        let mut p = progress.lock().await;
                        p.matched = matched_count;
                        p.total_queries = total_queries;
                        p.phase = phase;
                        p.not_found = scanned - matched_count;
                    }

                    // 同步持久化字段
                    sched.job.seed_cursor = sched.job.pair_cursor;

                    // 实时写入推算结果到 UI 共享变量
                    {
                        let mut r_lock = pred_results.lock().await;
                        *r_lock = sched.job.matched_pairs.iter()
                            .map(|p| PredictedRecord {
                                name: p.name.clone(),
                                shenfenzheng: p.sfz.clone(),
                                exam_number: p.exam_number.to_string(),
                                status: PredictedStatus::Matched,
                            })
                            .collect();
                    }

                    // 持久化任务
                    let save_err = crate::job::save_job(&sched.job).err();

                    {
                        let mut l = logs.lock().await;
                        if let Some(e) = save_err {
                            l.push(format!("⚠️ 保存任务进度失败: {}", e));
                        }
                        l.push(format!(
                            "📊 班级对={:?} | 游标={} | 扫描={} | 匹配={}/{} | 查询={}",
                            current_pair, sched.job.pair_cursor, scanned, matched_count, total_students, total_queries
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

    let _ = crate::job::save_job(&final_job);

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
            TaskType::ClassScan => "班级扫描",
            TaskType::Cleanup => "残留清扫",
        }
    }
}
