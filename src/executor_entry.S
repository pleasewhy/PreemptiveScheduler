# executor的入口函数
#
#   void executor_entry()
#
# 该函数只是简单的获取栈底的executor实例地址
# 然后再跳转到runtime.rs::run_executor函数。
#
  .section .text
  .globl executor_entry
  .balign 4
executor_entry:
  ld a0, 0(sp)
  j run_executor


