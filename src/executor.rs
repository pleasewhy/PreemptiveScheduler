extern crate alloc;
extern crate log;

use crate::context::Context as ExecuterContext;
use crate::runtime::Task;
use crate::waker_page::WAKER_PAGE_SIZE;
use alloc::alloc::{Allocator, Global, Layout};
use bit_iter::BitIter;
use core::pin::Pin;
use core::task::Waker;
use riscv::register::sstatus;
use trapframe::TrapFrame;
use core::matches;
use {
    alloc::boxed::Box,
    alloc::sync::Arc,
    core::future::Future,
    core::ptr::NonNull,
    core::task::{Context, Poll},
};

use crate::executor_entry;
use crate::runtime::PriorityInner;

enum ExecutorState {
    STRONG,
    WEAK, // 执行完一次future后就需要被drop
    KILLED,
    UNUSED,
}

pub struct Executor<F: Future<Output = ()> + Unpin> {
    priority_inner: Arc<PriorityInner<F>>,
    stack_base: usize,
    pub context: ExecuterContext,
    pub trapfrmae: TrapFrame,
    is_running_future: bool,
    state: ExecutorState,
}

const STACK_SIZE: usize = 4096 * 16;
impl<F: Future<Output = ()> + Unpin> Executor<F> {
    pub fn new(priority_inner: Arc<PriorityInner<F>>) -> Pin<Box<Self>> {
        unsafe {
            let layout = Layout::new::<[u8; STACK_SIZE]>();

            let stack_base: NonNull<u8> =
                Global.allocate(layout).expect("Alloction Failed.").cast();
            let stack_base = stack_base.as_ptr() as usize;
            let mut pin_executor = Pin::new(Box::new(Executor {
                priority_inner: priority_inner,
                stack_base: stack_base,
                context: ExecuterContext::default(),
                trapfrmae: TrapFrame::default(),
                is_running_future: false,
                state: ExecutorState::UNUSED,
            }));

            // 栈是从高地址向低地址生长的
            let stack_top = pin_executor.stack_base + STACK_SIZE;
            let executer_addr = pin_executor.as_ref().get_ref() as *const _ as usize;

            // 发生trap时, 会将trapframe储存在stack中, 在trap_return时
            // 会loadtrapframe恢复trap前的context. 对于新创建的executor,
            // 需要手动在栈上保存trapframe(主要是sepc和sstatus寄存器),
            // 并将sp寄存器设置为改地址.
            pin_executor.context.sp = Self::generate_stack(executer_addr, stack_top);

            // 将executor的入口地址设置为trap_return, 它会帮助我们sret到
            // executor_entry.S::executor_entry中执行
            pin_executor.context.ra = crate::trap_return as *const () as usize;

            log::warn!(
                "stack top 0x{:x} executor addr 0x{:x}",
                pin_executor.context.sp,
                executer_addr
            );
            pin_executor
        }
    }

    pub fn run(&mut self) {
        log::warn!("new executor run, addr={:x}", self as *const _ as usize);
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

                            unsafe {
                                sstatus::set_sie();
                            } // poll future时允许中断
                            self.is_running_future = true;

                            let ret = { Future::poll(pinned_ref, &mut cx) };

                            unsafe {
                                sstatus::clear_sie();
                            } // 禁用中断
                            self.is_running_future = false;

                            if let ExecutorState::WEAK = self.state {
                                log::info!("weak executor");
                                self.state = ExecutorState::KILLED;
                                return;
                            }

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
                log::warn!("yield");
                crate::runtime::yeild();
                unsafe {
                    crate::wait_for_interrupt();
                }
            }
        }
    }

    // stack layout: [executor_addr|32个寄存器|sstatus|sepc]
    // 发生中断时trapframe保存在栈中
    pub fn generate_stack(executor_addr: usize, stack_top: usize) -> usize {
        // 将executor的地址放置在栈底, 以便于新的executor执行线程获取到
        // executor实例。
        let mut stack_top = stack_top;
        stack_top -= core::mem::size_of::<usize>();
        unsafe {
            *(stack_top as *mut usize) = executor_addr;
        }

        let executor_stack = stack_top;
        stack_top -= core::mem::size_of::<TrapFrame>();
        unsafe {
            (*(stack_top as *mut TrapFrame)).general.sp = executor_stack;
            (*(stack_top as *mut TrapFrame)).sstatus = 1usize << 8; // 设置spp为supervisor
            (*(stack_top as *mut TrapFrame)).sepc = executor_entry as *const () as usize;
        }
        stack_top
    }

    // 当前是否在运行future
    // 发生supervisor时钟中断时, 若executor在运行future, 则
    // 说明该future超时, 需要切换到另一个executor来执行其他future.
    pub fn is_running_future(&self) -> bool {
        return self.is_running_future;
    }

    pub fn killed(&self) -> bool {
        return matches!(self.state, ExecutorState::KILLED);
    }

    pub fn mark_weak(&mut self) {
        self.state = ExecutorState::WEAK;
    }
}
unsafe impl Send for Executor<Task> {}
unsafe impl Sync for Executor<Task> {}
