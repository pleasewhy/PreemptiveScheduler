extern crate alloc;
extern crate log;

use crate::task_collection::Task;
use crate::waker_page::WAKER_PAGE_SIZE;
use crate::{context::Context as ExecuterContext, waker_page::WakerPageRef};
use alloc::alloc::{Allocator, Global, Layout};
use bit_iter::BitIter;
use core::matches;
use core::pin::Pin;
use core::task::Waker;
use riscv::register::sstatus;
use trapframe::TrapFrame;
use {
    alloc::boxed::Box,
    alloc::sync::Arc,
    core::future::Future,
    core::ptr::NonNull,
    core::task::{Context, Poll},
};

use crate::executor_entry;
use crate::task_collection::TaskCollection;

enum ExecutorState {
    STRONG,
    WEAK, // 执行完一次future后就需要被drop
    KILLED,
    UNUSED,
}

pub struct Executor {
    task_collection: Arc<TaskCollection<Task>>,
    stack_base: usize,
    pub context: ExecuterContext,
    pub trapfrmae: TrapFrame,
    is_running_future: bool,
    state: ExecutorState,
}

const STACK_SIZE: usize = 4096 * 32;
impl Executor {
    pub fn new(task_collection: Arc<TaskCollection<Task>>, cpu_id: u8) -> Pin<Box<Self>> {
        unsafe {
            let layout = Layout::new::<[u8; STACK_SIZE]>();

            let stack_base: NonNull<u8> =
                Global.allocate(layout).expect("Alloction Failed.").cast();
            let stack_base = stack_base.as_ptr() as usize;
            let mut pin_executor = Pin::new(Box::new(Executor {
                task_collection: task_collection,
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
            pin_executor.context.sp = Self::generate_stack(executer_addr, stack_top, cpu_id);

            // 将executor的入口地址设置为trap_return, 它会帮助我们sret到
            // executor_entry.S::executor_entry中执行
            pin_executor.context.ra = crate::trap_return as *const () as usize;

            log::trace!(
                "stack top 0x{:x} executor addr 0x{:x}",
                pin_executor.context.sp,
                executer_addr
            );
            pin_executor
        }
    }

    fn run_task(
        &mut self,
        key: u64,
        page_ref: WakerPageRef,
        pinned_task_ref: Pin<&mut Task>,
        waker: Waker,
    ) {
        let mut cx = Context::from_waker(&waker);
        let pinned_ptr = unsafe { Pin::into_inner_unchecked(pinned_task_ref) as *mut Task };
        let pinned_ref = unsafe { Pin::new_unchecked(&mut *pinned_ptr) };
        unsafe {
            sstatus::set_sie();
        } // poll future时允许中断
        self.is_running_future = true;

        log::trace!("polling future");
        let ret = { Future::poll(pinned_ref, &mut cx) };
        log::trace!("polling future over");
        unsafe {
            sstatus::clear_sie();
        } // 禁用中断
        self.is_running_future = false;

        if let ExecutorState::WEAK = self.state {
            log::info!("weak executor finish poll future, need killed");
            self.state = ExecutorState::KILLED;
            return;
        }

        match ret {
            Poll::Ready(()) => {
                // self.task_collection.remove_task(key);
                page_ref.mark_dropped((key % (WAKER_PAGE_SIZE as u64)) as usize);
            }
            Poll::Pending => (),
        }
    }

    pub fn run(&mut self) {
        log::trace!("new executor run, addr={:x}", self as *const _ as usize);
        loop {
            if let Some((key, page_ref, pinned_task_ref, waker)) =
                unsafe { Arc::get_mut_unchecked(&mut self.task_collection).take_task() }
            {
                self.run_task(key, page_ref, pinned_task_ref, waker)
            } else if let Some((key, page_ref, pinned_task_ref, waker)) =
                crate::runtime::take_task_from_other_cpu()
            {
                log::trace!("task from other cpu");
                self.run_task(key, page_ref, pinned_task_ref, waker)
            } else {
                // log::trace!("no future to run, need yield");
                crate::runtime::yeild();
                // log::trace!("yield over");
                // unsafe {
                //     crate::wait_for_interrupt();
                // }
            }
        }
    }

    // stack layout: [executor_addr|32个寄存器|sstatus|sepc]
    // 发生中断时trapframe保存在栈中
    pub fn generate_stack(executor_addr: usize, stack_top: usize, cpu_id: u8) -> usize {
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
            (*(stack_top as *mut TrapFrame)).general.tp = cpu_id as usize;
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
unsafe impl Send for Executor {}
unsafe impl Sync for Executor {}
