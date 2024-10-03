// Copyright (C) Microsoft Corporation. All rights reserved.

use crate::tests::common::run_lockable_test;
use crate::tests::common::LockTestBehavior;
use crate::tests::common::RFLAGS_ARITH_MASK;
use iced_x86::code_asm::*;
use x86emu::CpuState;

#[test]
fn cmpxchg() {
    let variations = [
        (0x0, 0x0, 0x1234, 0x0, 0x44),
        (0x64, 0x64, 0x1234, 0x64, 0x44),
        (0x0, 0x1, 0x0, 0x0, 0x0),
        (0x1, 0x0, 0x1, 0x1, 0x95),
        (
            0xffffffffffffffff,
            0x0,
            0xffffffffffffffff,
            0xffffffffffffffff,
            0x11,
        ),
        (
            0xffffffffffffffff,
            0xffffffff,
            0xffffffffffffffff,
            0xffffffffffffffff,
            0x5,
        ),
        (0xffffffff, 0xffffffffffffffff, 0xffffffff, 0xffffffff, 0x84),
        (0xffffffff, 0xffffffff, 0x1234, 0xffffffff, 0x44),
        (
            0x7fffffffffffffff,
            0x0,
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            0x91,
        ),
        (0x7fffffff, 0x0, 0x7fffffff, 0x7fffffff, 0x91),
        (0x0, 0x7fffffff, 0x0, 0x0, 0x4),
        (0x80000000, 0x7fffffff, 0x80000000, 0x80000000, 0x85),
        (0x7fffffff, 0x80000000, 0x7fffffff, 0x7fffffff, 0x10),
        (
            0x8000000000000000,
            0x7fffffff,
            0x8000000000000000,
            0x8000000000000000,
            0x885,
        ),
        (
            0x7fffffff,
            0x8000000000000000,
            0x7fffffff,
            0x7fffffff,
            0x810,
        ),
        (
            0x7fffffffffffffff,
            0x7fffffffffffffff,
            0x1234,
            0x7fffffffffffffff,
            0x44,
        ),
        (
            0x8000000000000000,
            0x7fffffffffffffff,
            0x8000000000000000,
            0x8000000000000000,
            0x885,
        ),
        (
            0x8000000000000000,
            0x8000000000000000,
            0x1234,
            0x8000000000000000,
            0x44,
        ),
    ];

    for (left, right, result_mem, result_areg, rflags) in variations {
        let (state, cpu) = run_lockable_test::<u64>(
            RFLAGS_ARITH_MASK,
            if left == result_mem {
                LockTestBehavior::Succeed
            } else {
                LockTestBehavior::Fail
            },
            |asm| asm.cmpxchg(qword_ptr(0x100), rbx),
            |state, cpu| {
                cpu.valid_gva = 0x100;
                cpu.mem_val = left;
                state.gps[CpuState::RAX] = right;
                state.gps[CpuState::RBX] = 0x1234;
            },
        );

        assert_eq!(cpu.mem_val, result_mem);
        assert_eq!(state.gps[CpuState::RAX], result_areg);
        assert_eq!(state.rflags & RFLAGS_ARITH_MASK, rflags.into());
    }
}
