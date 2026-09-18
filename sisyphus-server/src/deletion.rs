//! S3 产物异步删除执行器（票 #128，ADR-0026）。
//!
//! [`run_once`] 是生产后台循环与集成测试共用的深模块接口：认领、删除、
//! 失败记账、成功裁剪均隐藏在一次调用后面。

use std::time::Duration;

use crate::api::AppState;
use crate::store::deletions::DeletionScope;
use crate::store::{StoreError, now_ms};

/// 执行一个到期删除任务；无任务返回 `false`。S3 失败会持久化为 failed，
/// 本次仍返回 `Ok(true)`，避免一个外部故障终止后台循环。
pub async fn run_once(state: &AppState) -> Result<bool, StoreError> {
    let Some(job) = state.deletions.claim_next(now_ms()).await? else {
        return Ok(false);
    };
    let result: Result<(), String> = async {
        let keys = state
            .deletions
            .object_keys(&job)
            .await
            .map_err(|error| error.to_string())?;
        let multipart = state
            .deletions
            .multipart_uploads(&job)
            .await
            .map_err(|error| error.to_string())?;
        let pending = state
            .deletions
            .pending_artifact_objects(&job)
            .await
            .map_err(|error| error.to_string())?;
        if job.scope == DeletionScope::Project {
            for build_id in state
                .deletions
                .project_build_ids(job.project_id)
                .await
                .map_err(|error| error.to_string())?
            {
                crate::store::delete_build_data(&state.pool, state.artifacts.root(), build_id)
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
        let s3 = if keys.is_empty() && multipart.is_empty() && pending.is_empty() {
            None
        } else {
            Some(
                state
                    .s3
                    .as_ref()
                    .ok_or_else(|| "S3 后端未配置，无法删除对象".to_string())?,
            )
        };
        if let Some(s3) = s3 {
            for upload in multipart {
                if !upload.completed {
                    s3.abort_multipart_upload(&upload.object_key, &upload.upload_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                s3.delete_object(&upload.object_key)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            for artifact in pending {
                let blob = crate::storage::artifact_blob_name(
                    artifact.build_id,
                    artifact.job_id,
                    artifact.attempt,
                    &artifact.name,
                );
                let key = crate::storage::object_key(
                    s3.prefix(),
                    crate::storage::ObjectClass::Artifacts,
                    crate::storage::ObjectPhase::Temporary,
                    &blob,
                );
                s3.delete_object(&key)
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
        for key in keys {
            s3.expect("有对象键时已经要求 S3 后端")
                .delete_object(&key)
                .await
                .map_err(|error| error.to_string())?;
        }
        state
            .deletions
            .complete(&job)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        state.deletions.fail(job.id, &error).await?;
    }
    Ok(true)
}

/// 生产后台循环：每 30 秒唤醒，单轮排空当前可执行任务；失败任务由仓储层
/// 安排下次时间，不会紧密重试拖垮对象存储。
pub async fn run(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        loop {
            match run_once(&state).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(error) => {
                    tracing::warn!(error = %error, "异步产物删除扫描失败（下轮重试）");
                    break;
                }
            }
        }
    }
}
