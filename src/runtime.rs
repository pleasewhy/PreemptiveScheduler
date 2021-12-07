extern crate alloc;
use crate::context::Context as ExecutorContext;
use crate::executor::Executor;
use crate::switch;
use crate::task_collection::{Task, TaskCollection, TaskState, DEFAULT_PRIORITY, MAX_PRIORITY};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use lazy_static::*;
use riscv::asm;
use riscv::register::sstatus;
use spin::{Mutex, MutexGuard};
use {alloc::boxed::Box, core::future::Future, core::pin::Pin};

pub struct ExecutorRuntime<F: Future<Output = ()> + Unpin + 'static> {
    // runtime only run on this cpu
    cpu_id: u8,

    // 只会在一个core上运行，不需要考虑同步问题
    task_collection: Arc<TaskCollection<F>>,

    // 通过force_switch_future会将strong_executor降级为weak_executor
    strong_executor: Arc<Pin<Box<Executor<F>>>>,

    // 该executor在执行完一次后就会被drop
    weak_executor_vec: Vec<Option<Arc<Pin<Box<Executor<F>>>>>>,

    // 当前正在执行的executor
    current_executor: Option<Arc<Pin<Box<Executor<F>>>>>,

    // runtime context
    context: ExecutorContext,
}

impl<F: Future<Output = ()> + Unpin + 'static> ExecutorRuntime<F> {
    pub fn new(cpu_id: u8) -> Self {
        let task_collection = TaskCollection::new();
        let task_collection_cloned = task_collection.clone();
        let e = ExecutorRuntime {
            cpu_id: cpu_id,
            task_collection: task_collection,
            strong_executor: Arc::new(Executor::new(task_collection_cloned, cpu_id)),
            weak_executor_vec: vec![],
            current_executor: None,
            context: ExecutorContext::default(),
        };
        e
    }

    fn add_weak_executor(&mut self, weak_executor: Arc<Pin<Box<Executor<F>>>>) {
        self.weak_executor_vec.push(Some(weak_executor));
    }

    fn downgrade_strong_executor(&mut self) {
        let mut old = self.strong_executor.clone();
        unsafe {
            Arc::get_mut_unchecked(&mut old).mark_weak();
        }
        self.add_weak_executor(old);
        self.strong_executor = Arc::new(Executor::new(self.task_collection.clone(), self.cpu_id));
    }

    // return task number of current cpu.
    fn task_num(&self) -> usize {
        self.task_collection.task_num()
    }

    // 添加一个task，它的初始状态是notified，也就是说它可以被执行.
    fn add_task(&self, priority: usize, future: F) -> u64 {
        assert!(priority < MAX_PRIORITY);
        let key = self.task_collection.priority_insert(priority, future);
        key
    }

    fn remove_task(&self, key: u64) {
        self.task_collection.remove_task(key)
    }
}

impl<F: Future<Output = ()> + Unpin> Drop for ExecutorRuntime<F> {
    fn drop(&mut self) {
        log::error!("drop executor runtime!!!!");
    }
}

unsafe impl Send for ExecutorRuntime<Task> {}
unsafe impl Sync for ExecutorRuntime<Task> {}

lazy_static! {
    pub static ref GLOBAL_RUNTIME: [Mutex<ExecutorRuntime<Task>>; 2] = [
        Mutex::new(ExecutorRuntime::new(0)),
        Mutex::new(ExecutorRuntime::new(1))
    ];
}

// per-cpu scheduler.
pub fn run() {
    log::trace!("GLOBAL_RUNTIME.run()");
    loop {
        let mut runtime = get_current_runtime();
        let cx_ref = &runtime.context;
        let runtime_cx = cx_ref as *const ExecutorContext as usize;
        let executor_cx = &(runtime.strong_executor.context) as *const ExecutorContext as usize;
        // 释放保护global_runtime的锁
        runtime.current_executor = Some(runtime.strong_executor.clone());
        drop(runtime);
        unsafe {
            log::trace!("run strong executor");
            crate::switch(runtime_cx, executor_cx);
            // 该函数返回说明当前strong_executor执行的future超时了,
            // 需要重新创建一个executor执行后续的future, 并且将
            // 新的executor作为strong_executor，旧的executor添
            // 加到weak_exector中。
        }
        log::trace!("switch return");
        let mut runtime = get_current_runtime();

        // 只有strong_executor主动yield时, 才会执行运行weak_executor;
        if runtime.strong_executor.is_running_future() {
            runtime.downgrade_strong_executor();
            log::trace!("continued");
            continue;
        }

        // 遍历全部的weak_executor
        let weak_executor_num = runtime.weak_executor_vec.len();
        if 0 == weak_executor_num {
            drop(runtime);
            unsafe {
                crate::wait_for_interrupt();
            }
            continue;
        }
        log::trace!("run weak executor size={}", weak_executor_num);
        for i in 0..weak_executor_num {
            if runtime.weak_executor_vec[i].is_none() {
                continue;
            }
            let weak_executor = runtime.weak_executor_vec[i].as_ref().unwrap();
            if weak_executor.killed() {
                // TODO: 回收资源
                continue;
            }
            let executor_ctx = &weak_executor.context as *const ExecutorContext as usize;

            runtime.current_executor = Some(weak_executor.clone());
            drop(runtime);
            unsafe {
                // sstatus::set_sie();
                log::trace!("switch weak executor");
                switch(runtime_cx, executor_ctx);
                log::trace!("switch weak executor return");
                // sstatus::clear_sie();
            }
            log::trace!("global locking");
            runtime = get_current_runtime();
            log::trace!("global locking finish");
        }
        log::trace!("run weak executor finish");
    }
}

pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    log::trace!("spawn coroutine");
    priority_spawn(future, DEFAULT_PRIORITY);
    log::trace!("spawn coroutine over");
}

/// spawn a coroutine with `priority`.
/// insert new coroutine to the cpu with fewest number of tasks.
pub fn priority_spawn(future: impl Future<Output = ()> + Send + 'static, priority: usize) {
    log::trace!("in priority_spawn");
    let bf: Pin<alloc::boxed::Box<dyn Future<Output = ()> + Send + 'static>> = Box::pin(future);
    let future = Mutex::from(bf);
    let state = Mutex::from(TaskState::RUNNABLE);
    let task = Task {
        future,
        state,
        _priority: priority as u8,
    };

    let mut fewest_task_cpu_id = 0;
    let mut fewest_task_num = GLOBAL_RUNTIME[0].lock().task_num();
    for runtime_idx in 1..GLOBAL_RUNTIME.len() {
        let runtime = GLOBAL_RUNTIME[runtime_idx].lock();
        let task_num = runtime.task_num();
        if task_num < fewest_task_num {
            fewest_task_cpu_id = runtime_idx;
            fewest_task_num = task_num;
        }
    }
    log::trace!("fewest_task_cpu_id:{}", fewest_task_cpu_id);
    GLOBAL_RUNTIME[fewest_task_cpu_id]
        .lock()
        .add_task(priority, task);
}

/// check whether the running coroutine of current cpu time out, if yes, we will
/// switch to currrent cpu runtime that would create a new executor to run other
/// coroutines.
pub fn handle_timeout() {
    log::trace!("handle timeout");
    let runtime = get_current_runtime();
    if !runtime
        .current_executor
        .as_ref()
        .unwrap()
        .is_running_future()
    {
        return;
    }
    drop(runtime);
    yeild();
    log::trace!("handle timeout return");
}


// 运行executor.run()
#[no_mangle]
pub(crate) fn run_executor(executor_addr: usize) {
    log::trace!("run new executor: executor addr 0x{:x}", executor_addr);
    unsafe {
        let mut p = Box::from_raw(executor_addr as *mut Executor<Task>);
        p.run();

        let runtime = get_current_runtime();
        let cx_ref = &runtime.context;
        let executor_cx = &(p.context) as *const ExecutorContext as usize;
        let runtime_cx = cx_ref as *const ExecutorContext as usize;
        drop(runtime);
        unsafe { crate::switch(executor_cx, runtime_cx) }
        unreachable!();
    }
}

/// switch to runtime, which would select an appropriate executor to run.
pub(crate) fn yeild() {
    let runtime = get_current_runtime();
    let cx_ref = &runtime.context;
    let executor_cx =
        &(runtime.current_executor.as_ref().unwrap().context) as *const ExecutorContext as usize;
    let runtime_cx = cx_ref as *const ExecutorContext as usize;
    drop(runtime);
    unsafe { crate::switch(executor_cx, runtime_cx) }
}

/// return runtime `MutexGuard` of current cpu.
pub(crate) fn get_current_runtime() -> MutexGuard<'static, ExecutorRuntime<Task>> {
    GLOBAL_RUNTIME[crate::cpu_id() as usize].lock()
}
