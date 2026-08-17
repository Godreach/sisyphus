//! 占位模块：runner（ADR-0009 已列；真实执行随 B3-T3 实现）。
//!
//! B3-T1 只提供分派骨架的接收端：JobSpec / Cancel 指令经 channel reader
//! 投递到本模块通道，占位循环收帧即记日志（「占位 handle 可收」）。真实
//! 执行（host/container 后端、进程树终止、超时自保）随后续批次换入。

use sisyphus_proto::agent::{ChannelMessage, channel_message::Kind};
use tokio::sync::mpsc;

use crate::ReceiptLog;

/// runner 占位句柄：持有下行接收端与收帧观测（分派骨架断言面）。
pub struct Handle {
    rx: mpsc::Receiver<ChannelMessage>,
    receipts: ReceiptLog,
}

impl Handle {
    /// 以分派通道的接收端构造。
    pub fn new(rx: mpsc::Receiver<ChannelMessage>, receipts: ReceiptLog) -> Self {
        Self { rx, receipts }
    }

    /// 占位循环：收 JobSpec/Cancel 指令即记日志并忽略（执行随 B3-T3）。
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            let kind = match msg.kind {
                Some(Kind::JobSpec(_)) => "job_spec",
                Some(Kind::Cancel(_)) => "cancel",
                _ => "other",
            };
            tracing::info!(?msg, "runner 收到指令（占位：忽略，执行随后续批次）");
            self.receipts.lock().expect("观测锁").push(kind.to_string());
        }
    }
}
