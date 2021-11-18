#![no_std]
#![feature(allocator_api)]
#![feature(get_mut_unchecked)]
#![feature(global_asm)]

global_asm!(include_str!("switch.S"));
global_asm!(include_str!("executor_entry.S"));

extern "C" {
  pub(crate) fn wait_for_interrupt();
  pub(crate) fn executor_entry();
  pub(crate) fn switch(save_cx: usize, load_cx: usize);
  pub(crate) fn trap_return();
}

mod waker_page;
mod runtime;
mod executor;
mod context;

pub use runtime::{spawn, run, handle_timeout, test_borrow};