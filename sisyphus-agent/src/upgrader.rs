//! 占位模块：upgrader（ADR-0009 已列；自升级机制随 B3-T8 实现）。
//!
//! B3-T1 只提供分派骨架的接收端：UpgradeCommand 指令经 channel reader 投递
//! 到本模块通道，占位循环收帧即记日志（「占位 handle 可收」）。过旧版本
//! 「升级面保留」语义下的升级指令分发同样落在本模块通道。

use sisyphus_proto::agent::{ChannelMessage, channel_message::Kind};
use tokio::sync::mpsc;

use crate::ReceiptLog;

/// upgrader 占位句柄：持有下行接收端与收帧观测（分派骨架断言面）。
pub struct Handle {
    rx: mpsc::Receiver<ChannelMessage>,
    receipts: ReceiptLog,
}

impl Handle {
    /// 以分派通道的接收端构造。
    pub fn new(rx: mpsc::Receiver<ChannelMessage>, receipts: ReceiptLog) -> Self {
        Self { rx, receipts }
    }

    /// 占位循环：收升级指令即记日志并忽略（真实处理随 B3-T8）。
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            let kind = match msg.kind {
                Some(Kind::Upgrade(_)) => "upgrade",
                _ => "other",
            };
            tracing::info!(?msg, "upgrader 收到指令（占位：忽略，处理随后续批次）");
            self.receipts.lock().expect("观测锁").push(kind.to_string());
        }
    }
}
