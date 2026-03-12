use crate::compat::RzVec as Vec;

#[derive(Copy, Clone, Debug)]
pub(crate) struct StaticRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) writable: bool,
}

pub(crate) fn collect_static_ranges() -> Vec<StaticRange> {
    os::collect_static_ranges()
}

#[cfg(target_os = "macos")]
mod os {
    use super::StaticRange;
    use crate::compat::RzVec as Vec;
    use core::mem;
    use core::ptr;

    #[repr(C)]
    struct MachHeader64 {
        magic: u32,
        cputype: i32,
        cpusubtype: i32,
        filetype: u32,
        ncmds: u32,
        sizeofcmds: u32,
        flags: u32,
        reserved: u32,
    }

    #[repr(C)]
    struct LoadCommand {
        cmd: u32,
        cmdsize: u32,
    }

    #[repr(C)]
    struct SegmentCommand64 {
        cmd: u32,
        cmdsize: u32,
        segname: [u8; 16],
        vmaddr: u64,
        vmsize: u64,
        fileoff: u64,
        filesize: u64,
        maxprot: i32,
        initprot: i32,
        nsects: u32,
        flags: u32,
    }

    const MH_MAGIC_64: u32 = 0xfeedfacf;
    const LC_SEGMENT_64: u32 = 0x19;
    const VM_PROT_WRITE: i32 = 0x02;

    extern "C" {
        fn _dyld_image_count() -> u32;
        fn _dyld_get_image_header(image_index: u32) -> *const MachHeader64;
        fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    }

    pub(super) fn collect_static_ranges() -> Vec<StaticRange> {
        let mut ranges: Vec<StaticRange> = Vec::new();
        let count = unsafe { _dyld_image_count() };
        for i in 0..count {
            let header = unsafe { _dyld_get_image_header(i) };
            if header.is_null() {
                continue;
            }
            let header = unsafe { &*header };
            if header.magic != MH_MAGIC_64 {
                continue;
            }

            let slide = unsafe { _dyld_get_image_vmaddr_slide(i) };
            let mut cmd_ptr = unsafe {
                (header as *const MachHeader64 as *const u8).add(mem::size_of::<MachHeader64>())
            };

            for _ in 0..header.ncmds {
                let lc = unsafe { &*(cmd_ptr as *const LoadCommand) };
                if lc.cmd == LC_SEGMENT_64 && lc.cmdsize as usize >= mem::size_of::<SegmentCommand64>() {
                    let seg = unsafe { &*(cmd_ptr as *const SegmentCommand64) };
                    if seg.vmsize != 0 {
                        let start = (seg.vmaddr as isize).wrapping_add(slide) as usize;
                        let end = start.saturating_add(seg.vmsize as usize);
                        let writable = (seg.initprot & VM_PROT_WRITE) != 0;
                        if end > start {
                            ranges.push(StaticRange { start, end, writable });
                        }
                    }
                }
                let next = (lc.cmdsize as usize).max(mem::size_of::<LoadCommand>());
                cmd_ptr = unsafe { cmd_ptr.add(next) };
                if cmd_ptr == ptr::null() {
                    break;
                }
            }
        }
        ranges
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::StaticRange;
    use crate::compat::RzVec as Vec;
    use core::ffi::c_void;

    #[repr(C)]
    struct DlPhdrInfo {
        dlpi_addr: usize,
        dlpi_name: *const i8,
        dlpi_phdr: *const ElfPhdr,
        dlpi_phnum: u16,
    }

    #[repr(C)]
    struct ElfPhdr {
        p_type: u32,
        p_flags: u32,
        p_offset: u64,
        p_vaddr: u64,
        p_paddr: u64,
        p_filesz: u64,
        p_memsz: u64,
        p_align: u64,
    }

    const PT_LOAD: u32 = 1;
    const PF_W: u32 = 0x2;

    extern "C" {
        fn dl_iterate_phdr(
            callback: extern "C" fn(*const DlPhdrInfo, usize, *mut c_void) -> i32,
            data: *mut c_void,
        ) -> i32;
    }

    #[cfg(target_pointer_width = "64")]
    pub(super) fn collect_static_ranges() -> Vec<StaticRange> {
        extern "C" fn cb(info: *const DlPhdrInfo, _size: usize, data: *mut c_void) -> i32 {
            if info.is_null() {
                return 0;
            }
            let info = unsafe { &*info };
            let ranges = unsafe { &mut *(data as *mut Vec<StaticRange>) };
            let phdrs = info.dlpi_phdr;
            if phdrs.is_null() {
                return 0;
            }
            for idx in 0..info.dlpi_phnum as usize {
                let ph = unsafe { &*phdrs.add(idx) };
                if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
                    continue;
                }
                let start = info.dlpi_addr.saturating_add(ph.p_vaddr as usize);
                let end = start.saturating_add(ph.p_memsz as usize);
                let writable = (ph.p_flags & PF_W) != 0;
                if end > start {
                    ranges.push(StaticRange { start, end, writable });
                }
            }
            0
        }

        let mut ranges: Vec<StaticRange> = Vec::new();
        unsafe {
            dl_iterate_phdr(cb, &mut ranges as *mut _ as *mut c_void);
        }
        ranges
    }

    #[cfg(not(target_pointer_width = "64"))]
    pub(super) fn collect_static_ranges() -> Vec<StaticRange> {
        Vec::new()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod os {
    use super::StaticRange;
    use crate::compat::RzVec as Vec;
    pub(super) fn collect_static_ranges() -> Vec<StaticRange> {
        Vec::new()
    }
}
