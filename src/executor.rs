extern crate alloc;
extern crate log;

use crate::context::Context as ExecuterContext;
use crate::runtime::Task;
use crate::waker_page::WAKER_PAGE_SIZE;
use alloc::alloc::{Allocator, Global, Layout};
use bit_iter::BitIter;
use core::pin::Pin;
use core::task::Waker;
use {
    alloc::boxed::Box,
    alloc::sync::Arc,
    core::future::Future,
    core::ptr::NonNull,
    core::task::{Context, Poll},
};

use crate::executor_entry;
use crate::runtime::PriorityInner;

pub struct Executor<F: Future<Output = ()> + Unpin> {
    priority_inner: Arc<PriorityInner<F>>,
    stack_base: usize,
    pub context: ExecuterContext,
}

const STACK_SIZE: usize = 4096;
impl<F: Future<Output = ()> + Unpin> Executor<F> {
    pub fn new(priority_inner: Arc<PriorityInner<F>>) -> Pin<Box<Self>> {
        unsafe {
            let layout = Layout::new::<[u8; 4096]>();

            let stack_base: NonNull<u8> =
                Global.allocate(layout).expect("Alloction Failed.").cast();
            let stack_base = stack_base.as_ptr() as usize;
            let mut pin_executor = Pin::new(Box::new(Executor {
                priority_inner: priority_inner,
                stack_base: stack_base,
                context: ExecuterContext::default(),
            }));

            // 将executor的入口地址设置为executor_entry.asm::executor_entry
            pin_executor.context.ra = executor_entry as *const () as usize;

            // 栈是从高地址向低地址生长的
            let mut stack_top = pin_executor.stack_base + STACK_SIZE;

            // 将executor的地址放置在栈底, 以便于新的executor执行线程获取到
            // executor实例。
            stack_top -= core::mem::size_of::<usize>();
            let executer_ref = pin_executor.as_ref().get_ref();
            *(stack_top as *mut usize) = executer_ref as *const _ as usize;

            log::warn!(
                "stack top 0x{:x} executor addr 0x{:x}",
                stack_top,
                executer_ref as *const _ as usize
            );
            pin_executor.context.sp = stack_top;
            pin_executor
        }
    }

    pub fn run(&self) {
        loop {
            let mut found = false;
            for priority in 0..16 {
                let mut inner = self.priority_inner.get_mut_inner(priority);
                for page_idx in 0..inner.pages.len() {
                    let (notified, _dropped) = {
                        let page = &mut inner.pages[page_idx];
                        (page.take_notified(), page.take_dropped())
                    };
                    if notified != 0 {
                        found = true;
                        for subpage_idx in BitIter::from(notified) {
                            // task的idx
                            let idx = page_idx * WAKER_PAGE_SIZE + subpage_idx;
                            let waker = unsafe {
                                Waker::from_raw((&inner.pages[page_idx]).raw_waker(subpage_idx))
                            };
                            let mut cx = Context::from_waker(&waker);
                            let pinned_ref = inner.slab.get_pin_mut(idx).unwrap();
                            let pinned_ptr =
                                unsafe { Pin::into_inner_unchecked(pinned_ref) as *mut _ };
                            let pinned_ref = unsafe { Pin::new_unchecked(&mut *pinned_ptr) };
                            drop(inner); // 本次运行的coroutine可能会用到GLOBAL_EXCUTOR.inner(e.g. spawn())
                            let ret = { Future::poll(pinned_ref, &mut cx) };
                            inner = self.priority_inner.get_mut_inner(priority);
                            match ret {
                                Poll::Ready(()) => inner.pages[page_idx].mark_dropped(subpage_idx),
                                Poll::Pending => (),
                            }
                        }
                    }
                }
            }
            if !found {
                unsafe {
                    crate::wait_for_interrupt();
                }
            }
        }
    }
}

unsafe impl Send for Executor<Task> {}
unsafe impl Sync for Executor<Task> {}
