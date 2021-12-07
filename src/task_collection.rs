extern crate alloc;

use crate::waker_page::{WakerPage, WakerPageRef, WAKER_PAGE_SIZE};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bit_iter::BitIter;
use core::cell::RefCell;
use core::ops::{Generator, GeneratorState};
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::Waker;
use spin::{Mutex, MutexGuard};
use unicycle::pin_slab::PinSlab;

const TASK_NUM_PER_PRIORITY: u64 = 1 << 58;
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
    pub future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
    pub state: Mutex<TaskState>,
    pub _priority: u8,
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

impl<F: Future<Output = ()> + Unpin + 'static> Inner<F> {
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

    pub fn remove(&mut self, key: usize) {
        let (page, offset) = self.page(key as u64);
        page.clear(offset);
        self.slab.remove(key / WAKER_PAGE_SIZE);
    }
}

pub const DEFAULT_PRIORITY: usize = 4;
pub const MAX_PRIORITY: usize = 32;

pub struct TaskCollection<F: Future<Output = ()> + Unpin + 'static> {
    inners: Vec<RefCell<Inner<F>>>,
    task_num: AtomicUsize,
    generator:
        Option<Box<dyn Generator<Yield = Option<(u64, Pin<&'static mut F>, Waker)>, Return = ()>>>,
}

impl<F: Future<Output = ()> + Unpin + 'static> TaskCollection<F> {
    pub fn new() -> Arc<Self> {
        let mut task_collection = Arc::new(TaskCollection {
            inners: vec![],
            task_num: AtomicUsize::new(0),
            generator: None,
        });
        for _ in 0..MAX_PRIORITY {
            let inner = Inner {
                slab: PinSlab::new(),
                pages: vec![],
            };
            unsafe { Arc::get_mut_unchecked(&mut task_collection) }
                .inners
                .push(RefCell::new(inner));
        }

        unsafe { Arc::get_mut_unchecked(&mut task_collection) }.generator =
            Some(Box::new(TaskCollection::generator(task_collection.clone())));
        return task_collection;
    }

    /// return the `key` corresponding priority, `WakerPage` and offset.
    /// key layout:
    /// 0-6: offset in page.
    /// 6-59: page index in `TaskCollection.pages`.
    /// 59-64: priority.
    fn parse_key(&self, key: u64) -> (usize, &WakerPageRef, usize) {
        let key = key as usize;
        let subpage_ix = key % WAKER_PAGE_SIZE;
        let page_ix = (key << 5) >> 11;
        let priority = key >> 58;
        log::trace!("key= 0x{:x}", key);
        log::trace!("priority={}", priority);
        let ptr = self.inners[priority as usize].as_ptr();
        unsafe { (priority, &((*ptr).pages[page_ix]), subpage_ix) }
    }

    // 插入一个Future, 其优先级为 DEFAULT_PRIORITY
    pub fn insert(&self, future: F) -> u64 {
        self.priority_insert(DEFAULT_PRIORITY, future)
    }

    pub fn priority_insert(&self, priority: usize, future: F) -> u64 {
        assert!(priority < MAX_PRIORITY);
        let key = self.inners[priority].borrow_mut().insert(future);
        assert!(key < TASK_NUM_PER_PRIORITY);
        self.task_num.fetch_add(1, Ordering::Relaxed);
        key + ((priority as u64) * TASK_NUM_PER_PRIORITY)
    }

    fn get_mut_inner(&self, priority: usize) -> RefMut<'_, Inner<F>> {
        return self.inners[priority].try_borrow_mut().unwrap();
    }

    pub fn remove_task(&self, key: u64) {
        log::trace!("remove task key = 0x{:x}", key);
        let (priority, page, offset) = self.parse_key(key as u64);
        let mut inner = self.get_mut_inner(priority);
        page.mark_dropped(offset);
        inner.slab.remove((key % TASK_NUM_PER_PRIORITY) as usize);
        self.task_num.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn task_num(&self) -> usize {
        return self.task_num.load(Ordering::Relaxed);
    }

    pub fn take_task(&mut self) -> Option<(u64, Pin<&'static mut F>, Waker)> {
        unsafe {
            match Pin::new_unchecked(self.generator.as_mut().unwrap().as_mut()).resume(()) {
                GeneratorState::Yielded(ret) => ret,
                _ => panic!("unexpected value from resume"),
            }
        }
    }

    pub fn generator(
        self: Arc<Self>,
    ) -> impl Generator<Yield = Option<(u64, Pin<&'static mut F>, Waker)>, Return = ()> {
        static move || {
            loop {
                let mut found = false;
                for priority in 0..16 {
                    let mut inner = self.get_mut_inner(priority);
                    let pages_len = inner.pages.len();
                    for page_idx in 0..pages_len {
                        let (notified, _dropped) = {
                            let page = &mut inner.pages[page_idx];
                            (page.take_notified(), page.take_dropped())
                        };
                        log::trace!("notified={}", notified);
                        if notified != 0 {
                            found = true;
                            for subpage_idx in BitIter::from(notified) {
                                // the key corresponding to the task
                                let key = priority * (TASK_NUM_PER_PRIORITY as usize)
                                    + page_idx * WAKER_PAGE_SIZE
                                    + subpage_idx;
                                let waker = unsafe {
                                    Waker::from_raw((&inner.pages[page_idx]).raw_waker(subpage_idx))
                                };
                                let pinned_ref = inner
                                    .slab
                                    .get_pin_mut(key % (TASK_NUM_PER_PRIORITY as usize))
                                    .unwrap();
                                let pinned_ptr =
                                    unsafe { Pin::into_inner_unchecked(pinned_ref) as *mut F };
                                let pinned_ref = unsafe { Pin::new_unchecked(&mut *pinned_ptr) };
                                drop(inner);
                                log::trace!("yield coroutine");
                                yield Some((key as u64, pinned_ref, waker));
                                inner = self.get_mut_inner(priority);
                            }
                        }
                    }
                }
                if !found {
                    yield None;
                }
            }
        }
    }
}
