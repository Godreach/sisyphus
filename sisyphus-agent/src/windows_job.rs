//! Windows 作业对象（Job Object）进程树终止（ADR-0008「终止进程树」；
//! 票 B3-T5 / #59 的 Windows 强化）。
//!
//! `taskkill /T /F /PID` 依赖「父子关系」枚举进程树——shell（pwsh）异步
//! fork 的子进程（如 `ping`）在枚举窗口外尚未挂上父子关系时会被漏杀成
//! 孤儿，其残留进程持有管道句柄，会把等待者拖到自然退出（如 `ping -n 30`
//! 跑满 30s）。作业对象是 Windows 上整树终止的权威机制：子进程挂入 job
//! 后，其后代自动属同一 job，`TerminateJobObject` 一次杀干净（含已脱离
//! 父进程树的孤儿）。
//!
//! 本模块封装：spawn 后把子进程挂入新建 job（`KILL_ON_JOB_CLOSE` 兜底——
//! job 句柄关闭即杀全部成员，防句柄泄漏路径下进程残留）；kill 经
//! `TerminateJobObject`。挂入失败返回 `None`（调用侧回落 `taskkill /T`）。
//!
//! 全模块为 Windows 句柄 API 的 FFI 封装（`unsafe` 仅触及系统句柄，无内部
//! 数据访问），按仓库纪律统一标注。

#![allow(unsafe_code)] // windows-sys FFI：句柄 API 全部 unsafe，封装层集中标注

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_ALL_ACCESS};

/// 作业对象句柄（RAII：drop 时关闭——`KILL_ON_JOB_CLOSE` 确保成员进程
/// 在句柄关闭时被终止，防残留）。
pub struct JobHandle {
    raw: HANDLE,
}

impl JobHandle {
    /// `TerminateJobObject`：杀作业内全部进程（含异步 fork 的孤儿）。
    pub fn terminate(&self) {
        // 作业对象可能已被上次 terminate 杀空：重复杀无害（返回 0，忽略）。
        unsafe {
            TerminateJobObject(self.raw, 1);
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE：关闭句柄时终止作业内全部存活进程（兜底路径，
        // 正常路径已在 kill 时 terminate）。
        unsafe {
            CloseHandle(self.raw);
        }
    }
}

// HANDLE 是 `*mut c_void`（裸指针非自动 Send），但 Windows 句柄实为整数值
// 标识符——跨线程移动/共享安全（CloseHandle 可在任意线程调用）。显式标记
// 以让持有 job 的 SpawnedStep 满足 tokio 任务的 Send 约束（runner 跨 await
// 持有）。
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

/// 创建作业对象并把 `pid` 进程挂入。返回 `None` = 挂入失败（调用侧回落
/// `taskkill /T /F /PID`——不阻断 spawn，仅树终止能力降级）。
pub fn create_and_assign(pid: u32) -> Option<JobHandle> {
    unsafe {
        // 无名 job（句柄即引用，无需命名跨进程共享）。
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        // KILL_ON_JOB_CLOSE：job 句柄关闭时终止全部成员（防句柄泄漏残留）。
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return None;
        }
        // 打开子进程句柄并挂入 job。进程须未被其他 job 限制（代理/调试器等
        // 场景可能已入 job，Assign 失败——回落 taskkill）。
        let process = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
        if process.is_null() {
            CloseHandle(job);
            return None;
        }
        let assigned = AssignProcessToJobObject(job, process);
        CloseHandle(process);
        if assigned == 0 {
            CloseHandle(job);
            return None;
        }
        Some(JobHandle { raw: job })
    }
}
