# setjmp/longjmp through the recompiler's native unwind.
#
# The helpers have the same shape as the game's (setjmp 0x83438D70, longjmp
# 0x83438A50): setjmp saves LR, r1 and non-volatile registers in the buffer and
# returns 0; longjmp restores them, reloads r1 and LR, puts the value in r3 and
# returns through LR. The annotations below are what config/wwe14.toml says
# for the game.
#_ SETJMP my_setjmp
#_ LONGJMP_RETURN my_longjmp_return

my_setjmp:
  mflr r0
  stw r0, 0x134(r3)
  std r1, 0x90(r3)
  std r31, 0x98(r3)
  li r3, 0
  blr

my_longjmp:
  ld r31, 0x98(r3)
  lwz r5, 0x134(r3)
  mtlr r5
  ld r1, 0x90(r3)
  mr r3, r4
my_longjmp_return:
  blr

# longjmp from a nested call resumes after the setjmp call, with the value in
# r3 and the saved r31 restored.
test_longjmp_from_nested_call:
  #_ REGISTER_IN r1 0x10010000
  mflr r12
  stw r12, -8(r1)
  stwu r1, -0x40(r1)
  li r31, 7
  lis r3, 0x1000
  ori r3, r3, 0x2000
  bl my_setjmp
  cmpwi r3, 0
  bne resumed_1
  li r31, 99
  lis r3, 0x1000
  ori r3, r3, 0x2000
  li r4, 5
  bl jump_from_here
  li r30, 0xBAD
resumed_1:
  mr r29, r3
  addi r1, r1, 0x40
  lwz r12, -8(r1)
  mtlr r12
  blr
  #_ REGISTER_OUT r29 5
  #_ REGISTER_OUT r31 7
  #_ REGISTER_OUT r30 0
  #_ REGISTER_OUT r1 0x10010000

jump_from_here:
  mflr r0
  stw r0, -8(r1)
  stwu r1, -0x20(r1)
  bl my_longjmp
  addi r1, r1, 0x20
  lwz r0, -8(r1)
  mtlr r0
  blr

# An intermediate function that called setjmp on a different buffer must not
# catch a longjmp aimed at its caller's buffer.
test_longjmp_skips_inner_setjmp:
  #_ REGISTER_IN r1 0x10010000
  mflr r12
  stw r12, -8(r1)
  stwu r1, -0x40(r1)
  lis r3, 0x1000
  ori r3, r3, 0x2000
  bl my_setjmp
  cmpwi r3, 0
  bne resumed_2
  bl middle
  li r30, 0xBAD
resumed_2:
  mr r29, r3
  addi r1, r1, 0x40
  lwz r12, -8(r1)
  mtlr r12
  blr
  #_ REGISTER_OUT r29 9
  #_ REGISTER_OUT r28 1
  #_ REGISTER_OUT r27 0
  #_ REGISTER_OUT r30 0
  #_ REGISTER_OUT r1 0x10010000

middle:
  mflr r0
  stw r0, -8(r1)
  stwu r1, -0x30(r1)
  lis r3, 0x1000
  ori r3, r3, 0x3000
  bl my_setjmp
  cmpwi r3, 0
  bne middle_resumed
  li r28, 1
  lis r3, 0x1000
  ori r3, r3, 0x2000
  li r4, 9
  bl my_longjmp
middle_resumed:
  li r27, 0xBAD
  addi r1, r1, 0x30
  lwz r0, -8(r1)
  mtlr r0
  blr
