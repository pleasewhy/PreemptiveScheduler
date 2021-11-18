extern crate alloc;
use crate::context::Context as ExecutorContext;
use crate::executor::Executor;
use crate::switch;
use crate::waker_page::{WakerPage, WakerPageRef, WAKER_PAGE_SIZE};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use lazy_static::*;
use riscv::register::sstatus;
use log::warn;
use spin::Mutex;
use unicycle::pin_slab::PinSlab;
use {
    alloc::boxed::Box,
    core::cell::RefMut,
    core::future::Future,
    core::pin::Pin,
    core::task::{Context, Poll},
};

pub enum TaskState {
    _BLOCKED,
    RUNNABLE,
    _RUNNING,
}

pub struct Task {
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
    state: Mutex<TaskState>,
    _priority: u8,
}

impl Future for Task {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut f = self.future.lock();
        return f.as_mut().poll(cx);
    }
}

impl Task {
    pub fn _is_runnable(&self) -> bool {
        let task_state = self.state.lock();
        if let TaskState::RUNNABLE = *task_state {
            true
        } else {
            false
        }
    }

    pub fn _block(&self) {
        let mut task_state = self.state.lock();
        *task_state = TaskState::_BLOCKED;
    }
}

pub struct Inner<F: Future<Output = ()> + Unpin> {
    pub slab: PinSlab<F>,
    // root_waker: SharedWaker,
    pub pages: Vec<WakerPageRef>,
}

impl<F: Future<Output = ()> + Unpin> Inner<F> {
    /// Our pages hold 64 contiguous future wakers, so we can do simple arithmetic to access the
    /// correct page as well as the index within page.
    /// Given the `key` representing a future, return a reference to that page, `WakerPageRef`. And
    /// the index _within_ that page (usize).
    pub fn page(&self, key: u64) -> (&WakerPageRef, usize) {
        let key = key as usize;
        let (page_ix, subpage_ix) = (key / WAKER_PAGE_SIZE, key % WAKER_PAGE_SIZE);
        (&self.pages[page_ix], subpage_ix)
    }

    /// Insert a future into our scheduler returning an integer key representing this future. This
    /// key is used to index into the slab for accessing the future.
    pub fn insert(&mut self, future: F) -> u64 {
        let key = self.slab.insert(future);

        // Add a new page to hold this future's status if the current page is filled.
        while key >= self.pages.len() * WAKER_PAGE_SIZE {
            self.pages.push(WakerPage::new());
        }
        let (page, subpage_ix) = self.page(key as u64);
        page.initialize(subpage_ix);
        key as u64
    }
}

const DEFAULT_PRIORITY: usize = 4;
const MAX_PRIORITY: usize = 32;

pub struct PriorityInner<F: Future<Output = ()> + Unpin> {
    inners: Vec<RefCell<Inner<F>>>,
}

impl<F: Future<Output = ()> + Unpin> PriorityInner<F> {
    pub fn new() -> Self {
        let mut inner_vec = PriorityInner { inners: vec![] };
        for _ in 0..MAX_PRIORITY {
            let inner = Inner {
                slab: PinSlab::new(),
                pages: vec![],
            };
            inner_vec.inners.push(RefCell::new(inner));
        }
        return inner_vec;
    }

    fn page(&self, priority: usize, key: u64) -> (&WakerPageRef, usize) {
        let key = key as usize;
        let (page_ix, subpage_ix) = (key / WAKER_PAGE_SIZE, key % WAKER_PAGE_SIZE);
        let ptr = self.inners[priority as usize].as_ptr();
        unsafe { (&((*ptr).pages[page_ix]), subpage_ix) }
    }

    // 插入一个Future, 其优先级为 DEFAULT_PRIORITY
    fn insert(&self, future: F) {
        self.priority_insert(DEFAULT_PRIORITY, future);
    }

    fn priority_insert(&self, priority: usize, future: F) -> u64 {
        return self.inners[priority].borrow_mut().insert(future);
    }

    pub fn get_mut_inner(&self, priority: usize) -> RefMut<'_, Inner<F>> {
        return self.inners[priority].try_borrow_mut().unwrap();
    }
}

pub struct ExecutorRuntime<F: Future<Output = ()> + Unpin> {
    // 只会在一个core上运行，不需要考虑同步问题
    priority_inner: Arc<PriorityInner<F>>,

    // 通过force_switch_future会将strong_executor降级为weak_executor
    strong_executor: Arc<Pin<Box<Executor<F>>>>,

    // 该executor在执行完一次后就会被drop
    weak_executor_vec: Vec<Option<Arc<Pin<Box<Executor<F>>>>>>,

    // 当前正在执行的executor
    current_executor: Option<Arc<Pin<Box<Executor<F>>>>>,
    // runtime context
    context: ExecutorContext,
}

impl<F: Future<Output = ()> + Unpin> ExecutorRuntime<F> {
    pub fn new() -> Self {
        let priority_inner = Arc::new(PriorityInner::new());
        let priority_inner_cloned = priority_inner.clone();
        let e = ExecutorRuntime {
            priority_inner: priority_inner,
            strong_executor: Arc::new(Executor::new(priority_inner_cloned)),
            weak_executor_vec: vec![],
            current_executor: None,
            context: ExecutorContext::default(),
        };
        e
    }

    // 添加一个task，它的初始状态是notified，也就是说它可以被执行.
    fn add_task(&self, priority: usize, future: F) -> u64 {
        assert!(priority < MAX_PRIORITY);
        let mut inner = self.priority_inner.get_mut_inner(priority);
        let key = inner.insert(future);
        let (_page, _) = inner.page(key);
        key
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
        self.strong_executor = Arc::new(Executor::new(self.priority_inner.clone()));
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
    pub static ref GLOBAL_RUNTIME: Mutex<ExecutorRuntime<Task>> =
        Mutex::new(ExecutorRuntime::new());
}

pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    log::warn!("in spawn");
    return priority_spawn(future, DEFAULT_PRIORITY);
}

//
pub fn run() {
    log::warn!("GLOBAL_RUNTIME.run()");
    loop {
        let mut runtime = GLOBAL_RUNTIME.lock();
        let cx_ref = &runtime.context;
        let runtime_cx = cx_ref as *const ExecutorContext as usize;
        let executor_cx = &(runtime.strong_executor.context) as *const ExecutorContext as usize;
        // 释放保护global_runtime的锁
        runtime.current_executor = Some(runtime.strong_executor.clone());
        drop(runtime);
        unsafe {
            log::warn!("run strong executor");
            crate::switch(runtime_cx, executor_cx);
            // 该函数返回说明当前strong_executor执行的future超时了,
            // 需要重新创建一个executor执行后续的future, 并且将
            // 新的executor作为strong_executor，旧的executor添
            // 加到weak_exector中。
        }
        log::warn!("switch return");
        let mut runtime = GLOBAL_RUNTIME.lock();
        // 只有strong_executor主动yield时, 才会执行运行weak_executor;

        if runtime.strong_executor.is_running_future() {
            runtime.downgrade_strong_executor();
            log::warn!("continued");
            continue;
        }

        // 遍历全部的weak_executor
        let weak_executor_num = runtime.weak_executor_vec.len();
        log::warn!("run weak executor size={}", weak_executor_num);
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
                sstatus::set_sie();
                log::warn!("switch weak executor");
                switch(runtime_cx, executor_ctx);
                log::warn!("switch weak executor return");
                sstatus::clear_sie();
            }
            log::warn!("global locking");
            runtime = GLOBAL_RUNTIME.lock();
            log::warn!("global locking finish");
        }
        log::warn!("run weak executor finish");
    }
}

pub fn priority_spawn(future: impl Future<Output = ()> + Send + 'static, priority: usize) {
    log::warn!("in priority_spawn");
    let bf: Pin<alloc::boxed::Box<dyn Future<Output = ()> + Send + 'static>> = Box::pin(future);
    let future = Mutex::from(bf);
    let state = Mutex::from(TaskState::RUNNABLE);
    let task = Task {
        future,
        state,
        _priority: priority as u8,
    };
    GLOBAL_RUNTIME.lock().add_task(priority, task);
}

// 切换到runtime的context
pub fn handle_timeout() {
    let runtime = GLOBAL_RUNTIME.lock();
    if !runtime.current_executor.as_ref().unwrap().is_running_future() {
        return;
    }
    let cx_ref = &runtime.context;
    let executor_cx = &(runtime.strong_executor.context) as *const ExecutorContext as usize;
    let runtime_cx = cx_ref as *const ExecutorContext as usize;
    drop(runtime);
    log::warn!("switching to executor runtime");
    unsafe { crate::switch(executor_cx, runtime_cx) };
}

pub fn test_borrow() {
    let runtime = GLOBAL_RUNTIME.lock();
    let inner = runtime.priority_inner.get_mut_inner(DEFAULT_PRIORITY);
    log::warn!(
        "borrow successfully {:?}",
        inner.deref() as *const _ as usize
    );
}

// 运行executor.run()
#[no_mangle]
pub(crate) fn run_executor(executor_addr: usize) {
    log::warn!("executor addr {:x}", executor_addr);
    unsafe {
        let mut p = Box::from_raw(executor_addr as *mut Executor<Task>);
        p.run();
    }
    let runtime = GLOBAL_RUNTIME.lock();
    let cx_ref = &runtime.context;
    let executor_cx = &(runtime.strong_executor.context) as *const ExecutorContext as usize;
    let runtime_cx = cx_ref as *const ExecutorContext as usize;
    drop(runtime);
    unsafe { crate::switch(executor_cx, runtime_cx) }
    unreachable!();
}

pub(crate) fn yeild() {
    let runtime = GLOBAL_RUNTIME.lock();
    let cx_ref = &runtime.context;
    let executor_cx = &(runtime.strong_executor.context) as *const ExecutorContext as usize;
    let runtime_cx = cx_ref as *const ExecutorContext as usize;
    drop(runtime);
    unsafe { crate::switch(executor_cx, runtime_cx) }
}
