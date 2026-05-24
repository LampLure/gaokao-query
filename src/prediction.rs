use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, OwnedSemaphorePermit};

use crate::browser::BrowserClient;
use crate::data::*;

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
    ) -> Result<Arc<Self>, String> {
        let mut clients = VecDeque::with_capacity(count);
        for i in 0..count {
            let client = BrowserClient::new_with_log(
                false,
                logs.clone(),
                step_delay_ms,
                captcha_retries,
                captcha_wait_ms,
                hide_browser,
                target_url,
            )
            .await
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
            let _ = client.go_home().await;
            this.clients.lock().await.push_back(client);
            drop(permit);
        });
    }
}

pub async fn predict_student(
    pool: Arc<BrowserPool>,
    name: String,
    id_card: String,
    base_bkh: String,
    range_start: u64,
    range_end: u64,
    concurrency: usize,
    cancel: Arc<Mutex<bool>>,
    progress: Arc<Mutex<PredictionProgress>>,
) -> PredictedRecord {
    let mut exam = range_end;

    while exam >= range_start {
        if *cancel.lock().await {
            return PredictedRecord {
                name,
                shenfenzheng: id_card,
                exam_number: String::new(),
                status: PredictedStatus::NotFound,
            };
        }

        let batch_end = exam;
        let batch_start = if exam + 1 >= concurrency as u64 {
            exam - concurrency as u64 + 1
        } else {
            range_start
        };

        {
            let mut p = progress.lock().await;
            p.current_name = name.clone();
            p.current_exam = format!("{:05}", batch_start);
        }

        let mut handles = Vec::with_capacity(concurrency);
        for e in batch_start..=batch_end {
            if *cancel.lock().await {
                break;
            }
            let bkh = format!("{}{:05}", base_bkh, e);
            let id = id_card.clone();
            let pool = pool.clone();

            handles.push(tokio::spawn(async move {
                let (permit, client) = pool.acquire().await;
                let result = client.query_single(&bkh, &id).await;
                pool.release(permit, client);
                (e, result)
            }));
        }

        for handle in handles {
            if let Ok((e, result)) = handle.await {
                match result {
                    Ok(qr) if !qr.name.is_empty() => {
                        return PredictedRecord {
                            name,
                            shenfenzheng: id_card,
                            exam_number: format!("{}{:05}", base_bkh, e),
                            status: PredictedStatus::Matched,
                        };
                    }
                    _ => {}
                }
            }
        }

        if batch_start == range_start {
            break;
        }
        exam = batch_start - 1;
    }

    PredictedRecord {
        name,
        shenfenzheng: id_card,
        exam_number: String::new(),
        status: PredictedStatus::NotFound,
    }
}
