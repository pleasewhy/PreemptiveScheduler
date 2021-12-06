#![no_std]
#![feature(allocator_api)]
#![feature(get_mut_unchecked)]
#![feature(global_asm)]
#![feature(asm)]
#![feature(generators, generator_trait)]


global_asm!(include_str!("switch.S"));
global_asm!(include_str!("executor_entry.S"));

/// return id of current cpu, it requires kernel maintaining cpuid in tp
/// register.
pub(crate) fn cpu_id() -> u8 {
  let mut cpu_id;
  unsafe {
      asm!("mv {0}, tp", out(reg) cpu_id);
  }
  cpu_id
}

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
mod task_collection;

pub use runtime::{spawn, run, handle_timeout};