use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

use crate::browser::BrowserPool;
use crate::data::*;

/// ====================================================================
/// 【核心算法阶段一】分布式雷达盲推核心（方案 A：大步长考号广播 + 动态熔断）
/// ====================================================================
pub async fn run_radar_probes(
    pool: Arc<BrowserPool>,
    students: &Vec<(String, String)>, // 全班学生的 (姓名，身份证)
    base_bkh: &str,
    probes: Vec<(u64, u32)>,          // Vec<(探针号尾数，探针深度深度标签)>
    concurrency: usize,
    cancel_flag: Arc<Mutex<bool>>,
    logs: Arc<Mutex<Vec<String>>>,
) -> Option<(u64, u32)> { // 如果撞中，返回 (中奖号尾数，探针标签)

    // 构建 120 组雷达探测任务矩阵：3 个探针号 × 全班 N 个身份证
    let mut radar_tasks = Vec::new();
    for &(probe_bkh, depth) in &probes {
        let full_probe_string = format!("{}{}", base_bkh, probe_bkh);
        for (name, sfz) in students {
            radar_tasks.push((probe_bkh, full_probe_string.clone(), name.clone(), sfz.clone(), depth));
        }
    }

    // 内部多生产者单消费者通道，用于捕获首个成功碰撞的回波
    let (tx, mut rx) = mpsc::channel::<(u64, u32)>(10);
    let radar_cancel = Arc::new(Mutex::new(false)); // 内部批次熔断信号

    let mut worker_handles = Vec::new();
    let tasks_arc = Arc::new(Mutex::new(radar_tasks));

    for worker_id in 0..concurrency {
        let pool = pool.clone();
        let tasks = tasks_arc.clone();
        let tx = tx.clone();
        let inner_cancel = radar_cancel.clone();
        let global_cancel = cancel_flag.clone();
        let logs = logs.clone();

        worker_handles.push(tokio::spawn(async move {
            loop {
                // 检查外部中断或内部中奖熔断
                if *global_cancel.lock().await || *inner_cancel.lock().await { break; }

                // 领取探针任务
                let task = {
                    let mut t_lock = tasks.lock().await;
                    t_lock.pop()
                };
                let (probe_bkh, full_bkh, name, sfz, depth) = match task {
                    Some(t) => t,
                    None => break, // 任务领完，工人正常退出
                };

                // 获取空闲浏览器实例
                let (permit, client) = pool.acquire().await;
                
                // 执行一次撞击提交
                let result = client.query_single(&full_bkh, &sfz).await;
                
                // 关键点：雷达探测只关心是否"咬合"成功。如果顺利进入，且返回的姓名正确，即为中奖
                let is_hit = match &result {
                    Ok(query_res) => query_res.name == name,
                    Err(_) => false,
                };

                // 释放浏览器回首页，待命
                pool.release(permit, client);

                if is_hit {
                    // 触发内部批次熔断状态
                    {
                        let mut ic = inner_cancel.lock().await;
                        *ic = true;
                    }
                    let mut l = logs.lock().await;
                    l.push(format!("🎯 [雷达命中] 工人#{} 成功捕捉到目标！学生：{}, 考号：{}", worker_id, name, full_bkh));
                    
                    let _ = tx.send((probe_bkh, depth)).await;
                    break;
                }
            }
        }));
    }

    // 等待回波结果
    let hit_result = rx.recv().await;

    // 唤醒所有浏览器池实例强刷回首页待命，强行清洗终止当前批次残余动作
    {
        let mut ic = radar_cancel.lock().await;
        *ic = true; // 确保所有未退出的任务立刻死掉
    }
    
    // 保证子线程完全熔断
    for h in worker_handles { let _ = h.await; }

    hit_result
}

/// ====================================================================
/// 【核心算法阶段二】块状滑窗矩阵扫射核心（考号驱动 + 预备队列动态剔除瘦身）
/// ====================================================================
pub async fn run_matrix_sweep(
    pool: Arc<BrowserPool>,
    students: &mut Vec<(String, String)>, // 待瘦身的预备学生库（传入引用）
    base_bkh: &str,
    bkh_pool: Vec<u64>,                   // 产生的高密度珍珠串连续考号池
    concurrency: usize,
    cancel_flag: Arc<Mutex<bool>>,
    progress: Arc<Mutex<PredictionProgress>>,
    logs: Arc<Mutex<Vec<String>>>,
    perf_logs: Arc<Mutex<Vec<Vec<PerfEvent>>>>,
) -> Vec<PredictedRecord> {

    let mut final_records: Vec<PredictedRecord> = Vec::new();
    
    // 将待清查的学生身份证封装进共享的互斥线程安全池中
    let active_students = Arc::new(Mutex::new(students.clone()));
    let resolved_records = Arc::new(Mutex::new(Vec::new()));

    // 按照精髓："用一个账号去试剩下所有人"
    // 任务队列以考号为绝对驱动轴
    let bkh_queue = Arc::new(Mutex::new(bkh_pool));
    let mut handles = Vec::new();

    for worker_id in 0..concurrency {
        let pool = pool.clone();
        let bkh_queue = bkh_queue.clone();
        let active_students = active_students.clone();
        let resolved_records = resolved_records.clone();
        let global_cancel = cancel_flag.clone();
        let progress = progress.clone();
        let logs = logs.clone();
        let perf_logs = perf_logs.clone();
        let base_bkh = base_bkh.to_string();

        handles.push(tokio::spawn(async move {
            loop {
                if *global_cancel.lock().await { break; }

                // 1. 领用当前测试的考号
                let current_bkh_num = {
                    let mut q = bkh_queue.lock().await;
                    q.pop()
                };
                let current_bkh = match current_bkh_num {
                    Some(num) => num,
                    None => break, // 考号珍珠串全部清洗完，工人下班
                };

                let full_exam_number = format!("{}{}", base_bkh, current_bkh);

                // 2. 拷贝一份当前还未被解出的身份证死士名单
                let current_batch_students = {
                    let s_lock = active_students.lock().await;
                    s_lock.clone()
                };

                if current_batch_students.is_empty() { break; } // 全班已被全部救出，提前退场

                {
                    let mut p = progress.lock().await;
                    p.current_batch = format!("[扫射阶段] 正在测试考号 {}（全班剩余{}人待救）", full_exam_number, current_batch_students.len());
                }

                // 3. 驱动一个号，无序撞击这批待命的身份证名单
                for (name, sfz) in current_batch_students {
                    if *global_cancel.lock().await { break; }

                    // 检查该身份证是否已经被别的并发浏览器捷足先登解出了
                    {
                        let s_lock = active_students.lock().await;
                        if !s_lock.iter().any(|(_, s)| s == &sfz) { continue; } // 已被剔除，跳过
                    }

                    let (permit, mut client) = pool.acquire().await;
                    
                    let record_perf = Arc::new(Mutex::new(Vec::new()));
                    client.set_perf(Some(record_perf.clone()));
                    client.set_turbo(true); // 矩阵清洗属于已知连续快冲，强推暴力模式加速

                    let result = client.query_single(&full_exam_number, &sfz).await;

                    if let Ok(perf_data) = record_perf.try_lock() {
                        if !perf_data.is_empty() {
                            let mut pl = perf_logs.lock().await;
                            pl.push(perf_data.clone());
                        }
                    }
                    pool.release(permit, client);

                    // 判定是否精准狙击成功
                    if let Ok(res) = result {
                        if res.name == name {
                            // 1. 动态剔除瘦身：立刻从待清查名单里除名
                            {
                                let mut s_lock = active_students.lock().await;
                                s_lock.retain(|(_, s)| s != &sfz);
                            }

                            // 2. 更新进度条状态
                            {
                                let mut p = progress.lock().await;
                                p.matched += 1;
                                p.current_name = name.clone();
                                p.current_exam = full_exam_number.clone();
                            }

                            // 3. 归档成功数据
                            {
                                let mut r_lock = resolved_records.lock().await;
                                r_lock.push(PredictedRecord {
                                    name: name.clone(),
                                    shenfenzheng: sfz.clone(),
                                    exam_number: full_exam_number.clone(),
                                    status: PredictedStatus::Matched,
                                });
                            }

                            let mut l = logs.lock().await;
                            l.push(format!("✨ [狙击命中] 成功破解！学生：{} -> 考号：{}", name, full_exam_number));
                            break; // 一个考号只能属于一个学生，当前考号使命完成，跳出身份证碰撞流
                        }
                    }
                }
            }
        }));
    }

    // 等待所有扫射工人完成网格化清洗
    for h in handles { let _ = h.await; }

    // 收集最终被救出的全部成功记录
    let mut out_records = {
        let r = resolved_records.lock().await;
        r.clone()
    };

    // 那些在滑窗清扫完依旧没有任何响应的人，归类为 NotFound 状态补齐，防止表格缺失
    let final_left_students = active_students.lock().await;
    for (name, sfz) in final_left_students.iter() {
        out_records.push(PredictedRecord {
            name: name.clone(),
            shenfenzheng: sfz.clone(),
            exam_number: "扫射范围外或未登记".to_string(),
            status: PredictedStatus::NotFound,
        });
        let mut p = progress.lock().await;
        p.not_found += 1;
    }

    // 最终回写更新外层 App 里的可变学生名单，完成剪枝
    *students = final_left_students.clone();

    out_records
}
