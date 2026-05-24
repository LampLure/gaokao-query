use std::sync::Arc;
use tokio::sync::Mutex;

use crate::browser::BrowserPool;
use crate::data::*;

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
