# Registers as host locals (recomp::locals): what crosses calls.
#
# Each test is a small program of several functions. The answers are the same
# whether registers live in host locals or in the context (KAT_LOCALS=off).

# Helpers shaped like the game's __savegprlr / __restgprlr, with an entry per
# register. Calls to them are translated in place, as the game's are.
#_ INLINE save_29
#_ INLINE save_30
#_ INLINE save_31
#_ INLINE rest_29
#_ INLINE rest_30
#_ INLINE rest_31

save_29:
  std r29, -0x20(r1)
save_30:
  std r30, -0x18(r1)
save_31:
  std r31, -0x10(r1)
  stw r12, -0x8(r1)
  blr

rest_29:
  ld r29, -0x20(r1)
rest_30:
  ld r30, -0x18(r1)
rest_31:
  ld r31, -0x10(r1)
  lwz r12, -0x8(r1)
  mtlr r12
  blr

# A caller's preserved registers survive a callee that uses the same ones, and
# both give their own callers' values back (saves through the helpers).
test_preserved_across_call:
  #_ REGISTER_IN r1 0x10010000
  #_ REGISTER_IN r31 0x1111
  #_ REGISTER_IN r30 0x2222
  mflr r12
  bl save_30
  stwu r1, -0x60(r1)
  li r31, 5
  li r30, 6
  li r3, 1
  bl uses_r30_r31
  add r3, r3, r31
  add r3, r3, r30
  addi r1, r1, 0x60
  b rest_30
  #_ REGISTER_OUT r3 102
  #_ REGISTER_OUT r31 0x1111
  #_ REGISTER_OUT r30 0x2222
  #_ REGISTER_OUT r1 0x10010000

uses_r30_r31:
  mflr r12
  bl save_30
  stwu r1, -0x50(r1)
  li r31, 70
  li r30, 20
  add r3, r3, r31
  add r3, r3, r30
  addi r1, r1, 0x50
  b rest_30

# The same with saves written out (std/ld, stfd/lfd), and a callee without a frame.
test_inline_saves:
  #_ REGISTER_IN r1 0x10010000
  #_ REGISTER_IN r31 0xAAAA
  #_ REGISTER_IN f31 2.5
  #_ REGISTER_IN f1 1.5
  mflr r12
  stw r12, -0x8(r1)
  std r31, -0x10(r1)
  stfd f31, -0x18(r1)
  stwu r1, -0x70(r1)
  li r31, 3
  fmr f31, f1
  bl clobbers_inline
  mr r3, r31
  fmr f2, f31
  addi r1, r1, 0x70
  lwz r12, -0x8(r1)
  mtlr r12
  ld r31, -0x10(r1)
  lfd f31, -0x18(r1)
  blr
  #_ REGISTER_OUT r3 3
  #_ REGISTER_OUT f2 1.5
  #_ REGISTER_OUT r31 0xAAAA
  #_ REGISTER_OUT f31 2.5

clobbers_inline:
  std r31, -0x10(r1)
  stfd f31, -0x18(r1)
  li r31, 99
  fmr f31, f3
  ld r31, -0x10(r1)
  lfd f31, -0x18(r1)
  blr

# A callee outside the calling convention: it reads the caller's r31 (as an
# exception-handling funclet does) and hands a result back in r11.
test_callee_reads_callers_register:
  #_ REGISTER_IN r1 0x10010000
  mflr r12
  stw r12, -0x8(r1)
  std r31, -0x10(r1)
  stwu r1, -0x60(r1)
  li r31, 40
  bl reads_r31
  add r3, r3, r11
  addi r1, r1, 0x60
  lwz r12, -0x8(r1)
  mtlr r12
  ld r31, -0x10(r1)
  blr
  #_ REGISTER_OUT r3 49

reads_r31:
  addi r3, r31, 2
  li r11, 7
  blr

# A condition a callee leaves in CR0, tested by the caller.
test_condition_from_callee:
  #_ REGISTER_IN r1 0x10010000
  #_ REGISTER_IN r4 5
  mflr r12
  stw r12, -0x8(r1)
  stwu r1, -0x60(r1)
  bl compares_r4
  li r3, 0
  bne not_five
  li r3, 1
not_five:
  addi r1, r1, 0x60
  lwz r12, -0x8(r1)
  mtlr r12
  blr
  #_ REGISTER_OUT r3 1

compares_r4:
  cmpwi r4, 5
  blr

# CTR counts a loop with a call in it.
test_ctr_loop_with_call:
  #_ REGISTER_IN r1 0x10010000
  mflr r12
  stw r12, -0x8(r1)
  stwu r1, -0x60(r1)
  li r3, 0
  li r11, 4
  mtctr r11
again:
  bl adds_three
  bdnz again
  addi r1, r1, 0x60
  lwz r12, -0x8(r1)
  mtlr r12
  blr
  #_ REGISTER_OUT r3 12

adds_three:
  addi r3, r3, 3
  blr

# The carry a callee leaves is the one the caller's addze sees.
test_carry_from_callee:
  #_ REGISTER_IN r1 0x10010000
  #_ REGISTER_IN r4 1
  #_ REGISTER_IN r6 10
  mflr r12
  stw r12, -0x8(r1)
  stwu r1, -0x60(r1)
  addic r5, r4, -1
  bl clears_carry
  addze r3, r6
  addi r1, r1, 0x60
  lwz r12, -0x8(r1)
  mtlr r12
  blr
  #_ REGISTER_OUT r3 10

clears_carry:
  li r7, 0
  addic r7, r7, 0
  blr

# A call through a pointer: the callee preserves r31 for the caller.
#_ ADDRESS_TAKEN doubles
test_call_through_pointer:
  #_ REGISTER_IN r1 0x10010000
  #_ REGISTER_IN r31 0x77
  mflr r12
  stw r12, -0x8(r1)
  std r31, -0x10(r1)
  stwu r1, -0x60(r1)
  li r31, 21
  lis r11, doubles@h
  ori r11, r11, doubles@l
  mtctr r11
  mr r3, r31
  bctrl
  add r3, r3, r31
  addi r1, r1, 0x60
  lwz r12, -0x8(r1)
  mtlr r12
  ld r31, -0x10(r1)
  blr
  #_ REGISTER_OUT r3 63
  #_ REGISTER_OUT r31 0x77

doubles:
  std r31, -0x10(r1)
  li r31, 2
  mullw r3, r3, r31
  ld r31, -0x10(r1)
  blr

# A callee whose restore can't be followed (it pops its frame through a frame
# pointer): it is taken to restore r31, and checked.
test_frame_pointer_callee:
  #_ REGISTER_IN r1 0x10010000
  #_ REGISTER_IN r31 0x55
  mflr r12
  stw r12, -0x8(r1)
  std r31, -0x10(r1)
  stwu r1, -0x60(r1)
  li r31, 9
  li r3, 1
  bl with_frame_pointer
  add r3, r3, r31
  addi r1, r1, 0x60
  lwz r12, -0x8(r1)
  mtlr r12
  ld r31, -0x10(r1)
  blr
  #_ REGISTER_OUT r3 14
  #_ REGISTER_OUT r31 0x55

with_frame_pointer:
  mflr r12
  stw r12, -0x8(r1)
  std r31, -0x10(r1)
  addi r31, r1, -0x60
  stwu r1, -0x60(r1)
  stw r3, 0x40(r31)
  lwz r3, 0x40(r31)
  addi r3, r3, 4
  addi r1, r31, 0x60
  lwz r12, -0x8(r1)
  mtlr r12
  ld r31, -0x10(r1)
  blr

# A tail call to a function that takes a register outside the convention.
#_ FUNCTION adds_r11
test_tail_call_passes_register:
  #_ REGISTER_IN r3 10
  li r11, 5
  b adds_r11
  #_ REGISTER_OUT r3 15

adds_r11:
  add r3, r3, r11
  blr
