use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use tokio::sync::Mutex;
use crate::browser::BrowserPool;
use crate::data::{
    PredictionProgress, PerfEvent, BrowserStatus, CaptchaStats,
    QueryTask, TaskResult, TaskType, PredictionJob,
    StudentInfo, ScanPhase, Anchor,
    PredictedRecord, PredictedStatus,
};

/// 种子号码间距（每20号插一个种子）
const SEED_SPACING: u64 = 20;
/// 种子号码数量（100号范围内取5个种子）
const SEED_COUNT: usize = 5;
/// 种子搜索范围（从end_bkh往前搜索100号）
const SEED_RANGE: u64 = 100;

/// 扩展阶段：向两侧探测时的最大连续未命中次数
const EXPAND_MAX_MISS: usize = 5;

/// 跳跃探测步长（扩展阶段先大步探测边界）
const JUMP_STEP: u64 = 5;

/// 队列中最多保留的任务数（防止内存爆炸）
const MAX_QUEUE_SIZE: usize = 200;

// ═══════════════════════════════════════════════════════════
//  任务调度器：动态即时反馈版
//
//  核心改动（相比旧版）：
//  1. 工人每次拿1个任务，执行完立刻反馈
//  2. 命中后立刻剪枝：移除同考号冗余任务 + 移除已匹配学生
//  3. 智能探测序列：每个考号先只放1个学生，未命中再补下一个
//  4. 跳跃式扩展：先大步(step=5)探测边界，命中后细扫
//  5. 区域预估：根据班级人数估计区域大小
// ═══════════════════════════════════════════════════════════

struct TaskScheduler {
    job: PredictionJob,
    task_queue: VecDeque<QueryTask>,
    known_bkh: HashMap<String, u64>,

    // 两班递进扫描状态
    all_classes: Vec<u32>,
    current_pair: Vec<u32>,
    current_seeds: Vec<u64>,
    seed_hits: HashSet<u64>,
    expand_states: HashMap<u32, ExpandState>,

    // 智能探测：每个考号待尝试的学生队列
    // exam_number → 还没试过的学生列表
    probe_pending: HashMap<u64, VecDeque<StudentInfo>>,
    // 考号探测顺序
    probe_order: VecDeque<u64>,
}

/// 班级扩展状态
#[derive(Debug, Clone)]
struct ExpandState {
    /// 向左扩展：当前探测的偏移量
    left_offset: u64,
    /// 向左连续未命中次数
    left_miss: usize,
    /// 向左是否已确认到达边界
    left_done: bool,
    /// 向右扩展：当前探测的偏移量
    right_offset: u64,
    /// 向右连续未命中次数
    right_miss: usize,
    /// 向右是否已确认到达边界
    right_done: bool,
    /// 是否在跳跃探测阶段（先大步探测）
    jump_mode: bool,
}

impl ExpandState {
    fn new() -> Self {
        Self {
            left_offset: JUMP_STEP, // 跳跃起始步长
            left_miss: 0,
            left_done: false,
            right_offset: JUMP_STEP,
            right_miss: 0,
            right_done: false,
            jump_mode: true, // 先用跳跃模式
        }
    }

    fn is_done(&self) -> bool {
        self.left_done && self.right_done
    }
}

impl TaskScheduler {
    fn new(job: PredictionJob, known_bkh: HashMap<String, u64>) -> Self {
        let mut all_classes: Vec<u32> = job.unmatched_students.iter()
            .map(|s| s.class_num)
            .filter(|&c| c > 0)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        all_classes.sort_by(|a, b| b.cmp(a));

        let pair_idx = job.class_pair_idx;
        let current_pair: Vec<u32> = all_classes.iter()
            .skip(pair_idx * 2)
            .take(2)
            .copied()
            .collect();

        let pair_cursor = job.pair_cursor;
        let current_seeds = Self::calc_seed_numbers(pair_cursor, job.start_bkh);

        let mut scheduler = Self {
            job,
            task_queue: VecDeque::new(),
            known_bkh,
            all_classes,
            current_pair,
            current_seeds,
            seed_hits: HashSet::new(),
            expand_states: HashMap::new(),
            probe_pending: HashMap::new(),
            probe_order: VecDeque::new(),
        };

        scheduler.recover_seed_hits();
        scheduler
    }

    fn calc_seed_numbers(cursor: u64, start_bkh: u64) -> Vec<u64> {
        let mut seeds = Vec::new();
        for i in 0..SEED_COUNT {
            let num = cursor.saturating_sub(i as u64 * SEED_SPACING);
            if num < start_bkh { break; }
            seeds.push(num);
        }
        seeds
    }

    fn recover_seed_hits(&mut self) {
        for seed_num in &self.current_seeds {
            if self.job.anchors.iter().any(|a| a.exam_number == *seed_num) {
                self.seed_hits.insert(*seed_num);
            }
        }
        for seed_num in &self.current_seeds {
            if self.job.scanned_numbers.contains(seed_num) {
                self.seed_hits.insert(*seed_num);
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  核心：获取下一个任务（工人每次只拿1个）
    // ═══════════════════════════════════════════════════════════

    fn get_next_task(&mut self) -> Option<QueryTask> {
        // 优先从队列中取（已预生成的任务）
        if let Some(task) = self.task_queue.pop_front() {
            return Some(task);
        }

        // 队列空了，从智能探测序列中取
        if let Some(task) = self.pop_probe_task() {
            return Some(task);
        }

        // 探测序列也空了，根据当前阶段生成新任务
        self.refill_tasks();

        // 再次尝试
        if let Some(task) = self.task_queue.pop_front() {
            return Some(task);
        }
        if let Some(task) = self.pop_probe_task() {
            return Some(task);
        }

        None
    }

    /// 从智能探测序列中弹出1个任务
    fn pop_probe_task(&mut self) -> Option<QueryTask> {
        while let Some(exam_num) = self.probe_order.pop_front() {
            if let Some(students) = self.probe_pending.get_mut(&exam_num) {
                if let Some(student) = students.pop_front() {
                    let class_num = student.class_num;
                    let task_type = if self.current_seeds.contains(&exam_num) {
                        TaskType::SeedProbe
                    } else if self.job.phase == ScanPhase::PairExpand {
                        TaskType::ClassExpand
                    } else if self.job.phase == ScanPhase::PairScan {
                        TaskType::ClassScan
                    } else {
                        TaskType::Cleanup
                    };

                    // 如果这个考号还有学生要试，放回队尾（下次还会来取）
                    if !students.is_empty() {
                        self.probe_order.push_back(exam_num);
                    }

                    return Some(QueryTask {
                        exam_number: exam_num,
                        student_sfz: student.sfz,
                        student_name: student.name,
                        class_num,
                        task_type,
                    });
                }
            }
            // 这个考号的学生都试完了，清理
            self.probe_pending.remove(&exam_num);
        }
        None
    }

    /// 根据当前阶段重新填充任务
    fn refill_tasks(&mut self) {
        // 先用已知报考号做零成本匹配
        self.apply_known_bkh();

        // 如果所有学生都匹配了
        if self.job.unmatched_students.is_empty() {
            self.job.phase = ScanPhase::Completed;
            return;
        }

        match self.job.phase {
            ScanPhase::PairSeed => {
                self.generate_seed_probes();
                if self.probe_order.is_empty() && self.task_queue.is_empty() {
                    self.advance_to_expand();
                    self.generate_expand_probes();
                }
            }
            ScanPhase::PairExpand => {
                self.generate_expand_probes();
                if self.probe_order.is_empty() && self.task_queue.is_empty() {
                    self.advance_to_scan();
                    self.generate_scan_probes();
                }
            }
            ScanPhase::PairScan => {
                self.generate_scan_probes();
                if self.probe_order.is_empty() && self.task_queue.is_empty() {
                    self.advance_to_next_pair();
                    if self.job.phase == ScanPhase::PairSeed {
                        self.generate_seed_probes();
                    }
                }
            }
            ScanPhase::Cleanup => {
                self.generate_cleanup_tasks();
                if self.probe_order.is_empty() && self.task_queue.is_empty() {
                    self.job.phase = ScanPhase::Completed;
                }
            }
            ScanPhase::Completed => {}
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段1: 种子探测 — 每个种子号码用两班学生逐个试
    // ═══════════════════════════════════════════════════════════

    fn generate_seed_probes(&mut self) {
        if self.current_pair.is_empty() {
            return;
        }

        for &seed_num in &self.current_seeds {
            // 跳过已命中或已扫描的种子
            if self.seed_hits.contains(&seed_num) { continue; }
            if self.job.scanned_numbers.contains(&seed_num) { continue; }
            if self.job.matched_pairs.iter().any(|p| p.exam_number == seed_num) { continue; }
            if self.probe_pending.contains_key(&seed_num) { continue; }

            // 收集两班未匹配学生，交替排列（让不同班的学生穿插探测）
            let mut students = VecDeque::new();
            let unmatched_a = self.job.unmatched_of_class(self.current_pair[0]);
            let unmatched_b = if self.current_pair.len() > 1 {
                self.job.unmatched_of_class(self.current_pair[1])
            } else {
                Vec::new()
            };

            // 交替排列：A班1个、B班1个、A班1个...
            let mut ia = 0;
            let mut ib = 0;
            loop {
                let mut added = false;
                if ia < unmatched_a.len() {
                    students.push_back(unmatched_a[ia].clone());
                    ia += 1;
                    added = true;
                }
                if ib < unmatched_b.len() {
                    students.push_back(unmatched_b[ib].clone());
                    ib += 1;
                    added = true;
                }
                if !added { break; }
            }

            if !students.is_empty() {
                self.probe_pending.insert(seed_num, students);
                self.probe_order.push_back(seed_num);
            }
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段2: 跳跃扩展 — 从锚点向两侧大步探测 + 细扫
    //
    //  策略：
    //  1. 先用步长5跳跃探测，快速定位大致边界
    //  2. 跳跃命中后，回填中间号码细扫
    //  3. 连续未命中 EXPAND_MAX_MISS 次确认边界
    // ═══════════════════════════════════════════════════════════

    fn generate_expand_probes(&mut self) {
        let mut new_probes = Vec::new();

        for &class_num in &self.current_pair {
            let unmatched = self.job.unmatched_of_class(class_num);
            if unmatched.is_empty() { continue; }

            let class_anchors: Vec<&Anchor> = self.job.anchors.iter()
                .filter(|a| a.class_num == class_num)
                .collect();
            if class_anchors.is_empty() { continue; }

            let zone = self.job.class_zones.iter()
                .find(|z| z.class_num == class_num);

            let (left_bound, right_bound) = if let Some(z) = zone {
                (z.start_number, z.end_number)
            } else {
                let min = class_anchors.iter().map(|a| a.exam_number).min().unwrap_or(0);
                let max = class_anchors.iter().map(|a| a.exam_number).max().unwrap_or(0);
                (min, max)
            };

            let state = self.expand_states.entry(class_num).or_insert_with(ExpandState::new);
            if state.is_done() { continue; }

            // 根据班级人数预估区域大小
            let class_size = unmatched.len() + self.job.matched_pairs.iter()
                .filter(|p| p.class_num == class_num).count();
            let estimated_zone_size = (class_size as u64) * 12 / 10; // 1.2倍冗余

            // 向左扩展
            if !state.left_done {
                let step = if state.jump_mode { JUMP_STEP } else { 1 };
                let num = if left_bound >= step as u64 {
                    left_bound - state.left_offset
                } else {
                    state.left_done = true;
                    continue;
                };

                if num < self.job.start_bkh {
                    state.left_done = true;
                } else if Self::should_skip_number(&self.job, num) {
                    // 已扫描或已匹配，推进偏移
                    state.left_offset += step as u64;
                } else if !self.probe_pending.contains_key(&num) {
                    // 新号码，加入探测列表
                    let students: VecDeque<StudentInfo> = unmatched.iter()
                        .map(|s| (*s).clone())
                        .collect();
                    if !students.is_empty() {
                        new_probes.push((num, students, class_num));
                        state.left_offset += step as u64;

                        // 如果已经超出预估区域，增加未命中计数
                        let zone_width = right_bound - left_bound;
                        if zone_width > estimated_zone_size && state.left_offset > estimated_zone_size {
                            state.left_miss += 1;
                        }
                    }
                }
            }

            // 向右扩展
            if !state.right_done {
                let step = if state.jump_mode { JUMP_STEP } else { 1 };
                let num = right_bound + state.right_offset;

                if num > self.job.end_bkh {
                    state.right_done = true;
                } else if Self::should_skip_number(&self.job, num) {
                    state.right_offset += step as u64;
                } else if !self.probe_pending.contains_key(&num) {
                    let students: VecDeque<StudentInfo> = unmatched.iter()
                        .map(|s| (*s).clone())
                        .collect();
                    if !students.is_empty() {
                        new_probes.push((num, students, class_num));
                        state.right_offset += step as u64;

                        let zone_width = right_bound - left_bound;
                        if zone_width > estimated_zone_size && state.right_offset > estimated_zone_size {
                            state.right_miss += 1;
                        }
                    }
                }
            }

            // 检查跳跃模式是否应该切换到细扫模式
            if state.jump_mode {
                // 如果两侧都有命中，或者探测范围超过预估，切换到细扫
                let _left_hit = state.left_miss == 0 && state.left_offset > JUMP_STEP;
                let _right_hit = state.right_miss == 0 && state.right_offset > JUMP_STEP;
                let zone = self.job.class_zones.iter()
                    .find(|z| z.class_num == class_num);
                if let Some(z) = zone {
                    let zone_width = z.end_number - z.start_number;
                    if zone_width >= estimated_zone_size * 8 / 10 {
                        // 区域已经够大了，切换细扫
                        state.jump_mode = false;
                        state.left_offset = 1;
                        state.right_offset = 1;
                        eprintln!("[推算] {}班 切换到细扫模式 (区域宽={})", class_num, zone_width);
                    }
                }
            }
        }

        // 如果没有锚点的班级，用大范围探测
        if new_probes.is_empty() && self.probe_order.is_empty() {
            for &class_num in &self.current_pair {
                let unmatched = self.job.unmatched_of_class(class_num);
                if unmatched.is_empty() { continue; }

                let has_anchors = self.job.anchors.iter().any(|a| a.class_num == class_num);
                if has_anchors { continue; }

                // 没有锚点：在种子范围附近搜索
                let search_start = self.current_seeds.last().copied().unwrap_or(self.job.pair_cursor);
                let search_end = self.job.pair_cursor;

                let mut count = 0;
                for num in (search_start..=search_end).rev() {
                    if num < self.job.start_bkh { break; }
                    if Self::should_skip_number(&self.job, num) { continue; }
                    if self.probe_pending.contains_key(&num) { continue; }

                    let students: VecDeque<StudentInfo> = unmatched.iter()
                        .map(|s| (*s).clone())
                        .collect();
                    if !students.is_empty() {
                        new_probes.push((num, students, class_num));
                        count += 1;
                        if count >= 5 { break; }
                    }
                }
            }
        }

        // 将新探测加入队列
        for (num, students, _class_num) in new_probes {
            if self.probe_pending.len() >= MAX_QUEUE_SIZE { break; }
            self.probe_pending.insert(num, students);
            self.probe_order.push_back(num);
        }
    }

    /// 判断一个号码是否应该跳过（不借用 self，避免冲突）
    fn should_skip_number(job: &PredictionJob, num: u64) -> bool {
        job.scanned_numbers.contains(&num) ||
        job.matched_pairs.iter().any(|p| p.exam_number == num)
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段3: 区域扫描 — 在确认的班级区域内逐号探测
    // ═══════════════════════════════════════════════════════════

    fn generate_scan_probes(&mut self) {
        let mut new_probes = Vec::new();

        for &class_num in &self.current_pair {
            let unmatched = self.job.unmatched_of_class(class_num);
            if unmatched.is_empty() { continue; }

            let zone = self.job.class_zones.iter()
                .find(|z| z.class_num == class_num);

            let (zone_start, zone_end) = if let Some(z) = zone {
                (z.start_number, z.end_number)
            } else {
                // 没有区域：在种子范围附近搜索
                let search_start = self.job.pair_cursor.saturating_sub(SEED_RANGE + 200);
                let search_end = std::cmp::min(self.job.pair_cursor + 100, self.job.end_bkh);

                let mut count = 0;
                for num in (search_start..=search_end).rev() {
                    if num < self.job.start_bkh { continue; }
                    if Self::should_skip_number(&self.job, num) { continue; }
                    if self.probe_pending.contains_key(&num) { continue; }

                    let students: VecDeque<StudentInfo> = unmatched.iter()
                        .map(|s| (*s).clone())
                        .collect();
                    if !students.is_empty() {
                        new_probes.push((num, students, class_num));
                        count += 1;
                        if count >= 10 { break; }
                    }
                }
                continue;
            };

            // 在区域内逐号扫描
            let mut count = 0;
            for num in zone_start..=zone_end {
                if num < self.job.start_bkh { continue; }
                if Self::should_skip_number(&self.job, num) { continue; }
                if self.probe_pending.contains_key(&num) { continue; }

                let students: VecDeque<StudentInfo> = unmatched.iter()
                    .map(|s| (*s).clone())
                    .collect();
                if !students.is_empty() {
                    new_probes.push((num, students, class_num));
                    count += 1;
                    if count >= 50 { break; } // 每次最多生成50个号码的探测
                }
            }
        }

        for (num, students, _class_num) in new_probes {
            if self.probe_pending.len() >= MAX_QUEUE_SIZE { break; }
            self.probe_pending.insert(num, students);
            self.probe_order.push_back(num);
        }
    }

    // ═══════════════════════════════════════════════════════════
    //  阶段4: 残留清扫
    // ═══════════════════════════════════════════════════════════

    fn generate_cleanup_tasks(&mut self) {
        if self.job.unmatched_students.is_empty() {
            return;
        }

        let matched_numbers: HashSet<u64> = self.job.matched_pairs.iter()
            .map(|p| p.exam_number)
            .collect();

        let mut candidate_numbers: Vec<u64> = Vec::new();

        // 在班级区域边界附近搜索
        for zone in &self.job.class_zones {
            for offset in 1..=50u64 {
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

        let unmatched = self.job.unmatched_students.clone();
        let student_count = unmatched.len();

        for (i, &num) in candidate_numbers.iter().take(500).enumerate() {
            if self.probe_pending.contains_key(&num) { continue; }
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
    //  即时反馈：处理单个任务结果
    // ═══════════════════════════════════════════════════════════

    fn process_single_result(&mut self, result: &TaskResult) {
        self.job.scanned_numbers.insert(result.exam_number);
        self.job.total_queries += 1;

        if result.matched {
            self.job.record_match(
                &result.student_name,
                &result.student_sfz,
                result.exam_number,
                result.class_num,
                &result.kemumingcheng,
                &result.kaodianmingcheng,
            );

            // ① 命中！立刻移除该考号的所有剩余学生（不再试了）
            self.probe_pending.remove(&result.exam_number);
            // 从 probe_order 中也移除
            self.probe_order.retain(|&n| n != result.exam_number);

            // ② 从所有其他考号的待试列表中移除已匹配的学生
            let matched_sfz = &result.student_sfz;
            for students in self.probe_pending.values_mut() {
                students.retain(|s| s.sfz != *matched_sfz);
            }
            // 清理空的学生列表
            let empty_nums: Vec<u64> = self.probe_pending.iter()
                .filter(|(_, v)| v.is_empty())
                .map(|(&k, _)| k)
                .collect();
            for num in empty_nums {
                self.probe_pending.remove(&num);
                self.probe_order.retain(|&n| n != num);
            }

            // ③ 如果命中的是种子号码，标记
            if self.current_seeds.contains(&result.exam_number) {
                self.seed_hits.insert(result.exam_number);
            }

            // ④ 扩展阶段命中：重置未命中计数，更新扩展方向
            if result.task_type == TaskType::ClassExpand {
                if let Some(state) = self.expand_states.get_mut(&result.class_num) {
                    let zone = self.job.class_zones.iter()
                        .find(|z| z.class_num == result.class_num);
                    if let Some(z) = zone {
                        if result.exam_number < z.start_number {
                            state.left_miss = 0;
                            // 命中说明区域向左扩展了，跳跃探测生效，继续跳跃
                        } else if result.exam_number > z.end_number {
                            state.right_miss = 0;
                        }
                    }
                }
            }

            // ⑤ 也清除 task_queue 中针对该考号的冗余任务
            self.task_queue.retain(|t| t.exam_number != result.exam_number);

        } else {
            // 未命中：扩展阶段增加未命中计数
            if result.task_type == TaskType::ClassExpand {
                if let Some(state) = self.expand_states.get_mut(&result.class_num) {
                    let zone = self.job.class_zones.iter()
                        .find(|z| z.class_num == result.class_num);
                    if let Some(z) = zone {
                        if result.exam_number < z.start_number {
                            state.left_miss += 1;
                            if state.left_miss >= EXPAND_MAX_MISS {
                                state.left_done = true;
                                eprintln!("[推算] {}班 向左边界确认 (连续{}个未命中)", result.class_num, state.left_miss);
                            }
                        } else if result.exam_number > z.end_number {
                            state.right_miss += 1;
                            if state.right_miss >= EXPAND_MAX_MISS {
                                state.right_done = true;
                                eprintln!("[推算] {}班 向右边界确认 (连续{}个未命中)", result.class_num, state.right_miss);
                            }
                        }
                    }
                }
            }

            // 检查该考号是否所有学生都试完了（probe_pending 中为空或不存在）
            // 如果是，标记该号码为"扫描完毕"
            if let Some(students) = self.probe_pending.get(&result.exam_number) {
                if students.is_empty() {
                    self.probe_pending.remove(&result.exam_number);
                    self.probe_order.retain(|&n| n != result.exam_number);
                }
            }
        }

        // 更新班级区域统计
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

    // ═══════════════════════════════════════════════════════════
    //  阶段切换逻辑
    // ═══════════════════════════════════════════════════════════

    fn advance_to_expand(&mut self) {
        self.job.phase = ScanPhase::PairExpand;
        self.expand_states.clear();
        self.probe_pending.clear();
        self.probe_order.clear();
        eprintln!("[推算] 阶段推进: 种子探测 → 跳跃扩展 (班级: {:?})", self.current_pair);
    }

    fn advance_to_scan(&mut self) {
        self.job.phase = ScanPhase::PairScan;
        self.probe_pending.clear();
        self.probe_order.clear();
        eprintln!("[推算] 阶段推进: 跳跃扩展 → 区域扫描 (班级: {:?})", self.current_pair);
    }

    fn advance_to_next_pair(&mut self) {
        for &class_num in &self.current_pair {
            if !self.job.completed_class_nums.contains(&class_num) {
                self.job.completed_class_nums.push(class_num);
            }
        }

        self.job.class_pair_idx += 1;
        let pair_idx = self.job.class_pair_idx;
        let next_pair: Vec<u32> = self.all_classes.iter()
            .skip(pair_idx * 2)
            .take(2)
            .copied()
            .collect();

        if next_pair.is_empty() {
            if self.job.unmatched_students.is_empty() {
                self.job.phase = ScanPhase::Completed;
                eprintln!("[推算] 所有班级处理完成，全部匹配！");
            } else {
                self.job.phase = ScanPhase::Cleanup;
                eprintln!("[推算] 所有班级对处理完成，进入残留清扫 (剩余{}人)", self.job.unmatched_students.len());
            }
            return;
        }

        let current_min_exam = self.job.matched_pairs.iter()
            .filter(|p| self.current_pair.contains(&p.class_num))
            .map(|p| p.exam_number)
            .min();

        let new_cursor = if let Some(min_num) = current_min_exam {
            min_num.saturating_sub(SEED_RANGE)
        } else {
            self.job.pair_cursor.saturating_sub(SEED_RANGE)
        };

        self.job.pair_cursor = new_cursor.max(self.job.start_bkh);
        self.current_pair = next_pair;
        self.current_seeds = Self::calc_seed_numbers(self.job.pair_cursor, self.job.start_bkh);
        self.seed_hits.clear();
        self.expand_states.clear();
        self.probe_pending.clear();
        self.probe_order.clear();
        self.job.phase = ScanPhase::PairSeed;
        self.recover_seed_hits();

        eprintln!("[推算] 推进到下一对班级: {:?} | cursor={} | seeds={:?}",
            self.current_pair, self.job.pair_cursor, self.current_seeds);
    }

    fn apply_known_bkh(&mut self) {
        if self.known_bkh.is_empty() { return; }

        let known_pairs: Vec<(String, String, u64, u32)> = self.job.unmatched_students.iter()
            .filter_map(|s| {
                if let Some(&exam_num) = self.known_bkh.get(&s.name) {
                    Some((s.name.clone(), s.sfz.clone(), exam_num, s.class_num))
                } else {
                    None
                }
            })
            .collect();

        if !known_pairs.is_empty() {
            eprintln!("[推算] 已知报考号表匹配 {} 人（零成本）", known_pairs.len());
        }

        for (name, sfz, exam_num, class_num) in &known_pairs {
            self.job.record_match(name, sfz, *exam_num, *class_num, "", "");
        }
    }

    /// 获取当前进度摘要（供工人更新UI）
    fn get_progress_snapshot(&self) -> (usize, u64, String, usize, usize) {
        (
            self.job.matched_count,
            self.job.total_queries,
            self.job.phase.label().to_string(),
            self.job.scanned_numbers.len(),
            self.job.total_students,
        )
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
        l.push(format!("🚀 [动态即时反馈] 启动！任务={} | 学生数={} | 已匹配={} | 阶段={}",
            job_name, total_students, job.matched_count, job.phase.label()));
        l.push(format!("   策略：种子探测 → 跳跃扩展 → 区域扫描 → 下一对"));
        l.push(format!("   优化：即时反馈 + 动态剪枝 + 跳跃边界探测"));
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
            // 每个工人持有一个浏览器实例
            let (permit, mut client) = match pool.acquire().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[Worker#{}] 获取浏览器失败: {}", worker_id, e);
                    return;
                }
            };
            client.set_captcha_stats(Some(captcha_stats.clone()));
            client.set_status(Some(browser_statuses.clone()));
            client.set_turbo(true);
            let record_perf = Arc::new(Mutex::new(Vec::new()));
            client.set_perf(Some(record_perf.clone()));

            loop {
                if cancel_flag.load(AtomicOrdering::Relaxed) { break; }
                if pool.is_shutdown() { break; }

                // ① 拿1个任务
                let task = {
                    let mut sched = scheduler.lock().await;
                    sched.get_next_task()
                };

                let task = match task {
                    Some(t) => t,
                    None => break, // 所有任务完成
                };

                let full_exam_number = task.exam_number.to_string();

                // ② 更新进度
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

                // ③ 执行查询
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

                let (kemumingcheng, kaodianmingcheng) = match &result {
                    Ok(res) => (res.kemumingcheng.clone(), res.kaodianmingcheng.clone()),
                    Err(_) => (String::new(), String::new()),
                };

                let task_result = TaskResult {
                    exam_number: task.exam_number,
                    student_sfz: task.student_sfz.clone(),
                    student_name: task.student_name.clone(),
                    class_num: task.class_num,
                    task_type: task.task_type.clone(),
                    matched,
                    error,
                    kemumingcheng,
                    kaodianmingcheng,
                };

                // ④ 即时反馈！立刻处理结果
                {
                    let mut sched = scheduler.lock().await;
                    sched.process_single_result(&task_result);

                    // 更新进度
                    let (matched_count, total_queries, phase, scanned, _) =
                        sched.get_progress_snapshot();

                    {
                        let mut p = progress.lock().await;
                        p.matched = matched_count;
                        p.total_queries = total_queries;
                        p.phase = phase;
                        p.not_found = scanned.saturating_sub(matched_count);
                    }

                    // 同步持久化
                    sched.job.seed_cursor = sched.job.pair_cursor;

                    // 实时写入推算结果
                    {
                        let mut pairs: Vec<_> = sched.job.matched_pairs.iter()
                            .map(|p| PredictedRecord {
                                name: p.name.clone(),
                                shenfenzheng: p.sfz.clone(),
                                exam_number: p.exam_number.to_string(),
                                class_num: p.class_num,
                                kemumingcheng: p.kemumingcheng.clone(),
                                kaodianmingcheng: p.kaodianmingcheng.clone(),
                                status: PredictedStatus::Matched,
                            })
                            .collect();
                        pairs.sort_by(|a, b| {
                            a.class_num.cmp(&b.class_num)
                                .then_with(|| a.exam_number.cmp(&b.exam_number))
                        });
                        let mut r_lock = pred_results.lock().await;
                        *r_lock = pairs;
                    }

                    // 持久化任务（每10次查询保存一次，减少IO）
                    if total_queries % 10 == 0 {
                        let _ = crate::job::save_job(&sched.job);
                    }
                }

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
            class_num: pair.class_num,
            kemumingcheng: pair.kemumingcheng.clone(),
            kaodianmingcheng: pair.kaodianmingcheng.clone(),
            status: PredictedStatus::Matched,
        });
    }

    for student in &final_job.unmatched_students {
        out_records.push(PredictedRecord {
            name: student.name.clone(),
            shenfenzheng: student.sfz.clone(),
            exam_number: "扫描范围外".to_string(),
            class_num: student.class_num,
            kemumingcheng: String::new(),
            kaodianmingcheng: String::new(),
            status: PredictedStatus::NotFound,
        });
    }

    out_records.sort_by(|a, b| {
        a.class_num.cmp(&b.class_num)
            .then_with(|| a.exam_number.cmp(&b.exam_number))
    });

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
            TaskType::ClassExpand => "跳跃扩展",
            TaskType::ClassScan => "区域扫描",
            TaskType::Cleanup => "残留清扫",
        }
    }
}
