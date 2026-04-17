use kvm_bindings::*;
use std::hint::black_box;

const MAX_FAM_ENTRIES: usize = 8;

struct Cursor<'a> {
    data: &'a [u8],
    idx: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, idx: 0 }
    }

    fn byte(&mut self) -> u8 {
        if self.idx >= self.data.len() {
            0
        } else {
            let out = self.data[self.idx];
            self.idx += 1;
            out
        }
    }

    fn bounded_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.byte() as usize) % bound
        }
    }

    fn u32(&mut self) -> u32 {
        let b0 = self.byte() as u32;
        let b1 = self.byte() as u32;
        let b2 = self.byte() as u32;
        let b3 = self.byte() as u32;
        b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
    }

    fn u64(&mut self) -> u64 {
        let lo = self.u32() as u64;
        let hi = self.u32() as u64;
        lo | (hi << 32)
    }
}

#[cfg(target_arch = "x86_64")]
fn fuzz_x86_bindings(cursor: &mut Cursor<'_>) {
    let mut regs = kvm_regs::default();
    regs.rax = cursor.u64();
    regs.rbx = cursor.u64();
    regs.rcx = cursor.u64();
    regs.rdx = cursor.u64();
    regs.rip = cursor.u64();
    regs.rflags = cursor.u64() | 2;
    black_box(
        regs.rax
            ^ regs.rbx
            ^ regs.rcx
            ^ regs.rdx
            ^ regs.rip
            ^ regs.rflags,
    );

    let mut sregs = kvm_sregs::default();
    sregs.cs.base = cursor.u64();
    sregs.cs.limit = cursor.u32();
    sregs.cs.selector = cursor.u16();
    sregs.cr0 = cursor.u64();
    sregs.cr3 = cursor.u64();
    sregs.cr4 = cursor.u64();
    black_box(
        sregs.cs.base
            ^ u64::from(sregs.cs.limit)
            ^ u64::from(sregs.cs.selector)
            ^ sregs.cr0
            ^ sregs.cr3
            ^ sregs.cr4,
    );

    let cpuid_count = cursor.bounded_usize(MAX_FAM_ENTRIES + 1);
    let mut cpuid_entries = Vec::with_capacity(cpuid_count);
    for idx in 0..cpuid_count {
        let mut entry = kvm_cpuid_entry2::default();
        entry.function = cursor.u32();
        entry.index = idx as u32;
        entry.flags = cursor.u32();
        entry.eax = cursor.u32();
        entry.ebx = cursor.u32();
        entry.ecx = cursor.u32();
        entry.edx = cursor.u32();
        cpuid_entries.push(entry);
    }
    if let Ok(mut cpuid) = CpuId::from_entries(&cpuid_entries) {
        let slice = cpuid.as_mut_slice();
        if !slice.is_empty() {
            let idx = cursor.bounded_usize(slice.len());
            slice[idx].eax ^= cursor.u32();
        }
        let mut acc = 0u32;
        for entry in slice.iter().take(4) {
            acc ^= entry.function ^ entry.index ^ entry.flags ^ entry.eax ^ entry.ebx ^ entry.ecx ^ entry.edx;
        }
        black_box(acc ^ cpuid.as_slice().len() as u32);
    }

    let msr_count = cursor.bounded_usize(MAX_FAM_ENTRIES + 1);
    let mut msr_entries = Vec::with_capacity(msr_count);
    for _ in 0..msr_count {
        let mut entry = kvm_msr_entry::default();
        entry.index = cursor.u32();
        entry.data = cursor.u64();
        msr_entries.push(entry);
    }
    if let Ok(mut msrs) = Msrs::from_entries(&msr_entries) {
        let slice = msrs.as_mut_slice();
        if !slice.is_empty() {
            let idx = cursor.bounded_usize(slice.len());
            slice[idx].data ^= cursor.u64();
        }
        let mut acc = 0u64;
        for entry in slice.iter().take(4) {
            acc ^= u64::from(entry.index) ^ entry.data;
        }
        black_box(acc ^ msrs.as_slice().len() as u64);
    }

    let routing_count = cursor.bounded_usize(MAX_FAM_ENTRIES + 1);
    if let Ok(mut routing) = KvmIrqRouting::new(routing_count) {
        let slice = routing.as_mut_slice();
        for entry in slice.iter_mut() {
            entry.gsi = cursor.u32();
            entry.type_ = cursor.u32();
            entry.flags = cursor.u32();
        }
        let mut acc = 0u32;
        for entry in slice.iter().take(4) {
            acc ^= entry.gsi ^ entry.type_ ^ entry.flags;
        }
        black_box(acc ^ routing.as_slice().len() as u32);
    }

    let mut mp_state = kvm_mp_state::default();
    mp_state.mp_state = cursor.u32();
    black_box(mp_state.mp_state);

    let mut events = kvm_vcpu_events::default();
    events.exception.nr = cursor.byte();
    events.exception.has_error_code = cursor.byte();
    events.exception.error_code = cursor.u32();
    black_box(
        u64::from(events.exception.nr)
            ^ u64::from(events.exception.has_error_code)
            ^ u64::from(events.exception.error_code),
    );
}

#[cfg(not(target_arch = "x86_64"))]
fn fuzz_x86_bindings(_cursor: &mut Cursor<'_>) {}

trait CursorExt {
    fn u16(&mut self) -> u16;
}

impl CursorExt for Cursor<'_> {
    fn u16(&mut self) -> u16 {
        let b0 = self.byte() as u16;
        let b1 = self.byte() as u16;
        b0 | (b1 << 8)
    }
}

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut cursor = Cursor::new(&data);
    fuzz_x86_bindings(&mut cursor);
}
