#![no_std]
#![no_main]
#![feature(naked_functions, asm_sym, asm_const, const_btree_new)]
#![feature(default_alloc_error_handler)]
#![deny(warnings)]

mod mm;
mod process;

#[macro_use]
extern crate output;

#[macro_use]
extern crate alloc;

use crate::{
    impls::{Sv39Manager, SyscallContext, Console},
    process::{Process, PIDALLOCATOR},
};
use kernel_context::foreign::ForeignPortal;
use kernel_vm::{
    page_table::{MmuMeta, Sv39, VAddr, VmFlags, PPN, VPN},
    AddressSpace,
};
use output::log;
use riscv::register::*;
use sbi_rt::*;
use syscall::Caller;
use xmas_elf::ElfFile;
use task_manage::TaskManager;
// use lazy_static::lazy_static;

// 应用程序内联进来。
core::arch::global_asm!(include_str!(env!("APP_ASM")));

/// Supervisor 汇编入口。
///
/// 设置栈并跳转到 Rust。
#[naked]
#[no_mangle]
#[link_section = ".text.entry"]
unsafe extern "C" fn _start() -> ! {
    const STACK_SIZE: usize = 16 * 4096;

    #[link_section = ".bss.uninit"]
    static mut STACK: [u8; STACK_SIZE] = [0u8; STACK_SIZE];

    core::arch::asm!(
        "la sp, {stack} + {stack_size}",
        "j  {main}",
        stack_size = const STACK_SIZE,
        stack      =   sym STACK,
        main       =   sym rust_main,
        options(noreturn),
    )
}

// lazy_static! {
//     static ref TASKMANAGER: TaskManager<Process> = TaskManager::new();
// }


static mut TASKMANAGER: TaskManager<Process> = TaskManager::new();


extern "C" fn rust_main() -> ! {
    // bss 段清零
    utils::zero_bss();
    // 初始化 `output`
    output::init_console(&Console);
    output::set_log_level(option_env!("LOG"));
    output::test_log();
    // 初始化 syscall
    syscall::init_io(&SyscallContext);
    syscall::init_process(&SyscallContext);
    syscall::init_scheduling(&SyscallContext);
    syscall::init_clock(&SyscallContext);

    // 初始化内核堆
    mm::init();
    mm::test();
    // 建立内核地址空间
    let mut ks = kernel_space();
    // 异界传送门
    // 可以直接放在栈上
    let mut portal = ForeignPortal::new();
    let tramp = (
        PPN::<Sv39>::new(&portal as *const _ as usize >> Sv39::PAGE_BITS),
        VmFlags::build_from_str("XWRV")
    );
    // 传送门映射到所有地址空间
    ks.map_portal(tramp);
    // 加载应用程序
    extern "C" {
        static apps: utils::AppMeta;
    }
    for (i, elf) in unsafe { apps.iter_elf() }.enumerate() {
        let base = elf.as_ptr() as usize;
        log::info!("detect app[{i}]: {base:#x}..{:#x}", base + elf.len());
        if let Some(mut process) = Process::from_elf(ElfFile::new(elf).unwrap()) {
            process.address_space.map_portal(tramp);
            unsafe {
                TASKMANAGER.insert(process.pid, process);
            };
        }
    }
    const PROTAL_TRANSIT: usize = VPN::<Sv39>::MAX.base().val();
    loop {

        if let Some(task) = unsafe { TASKMANAGER.fetch() }{
            task.execute(&mut portal, PROTAL_TRANSIT);
            match scause::read().cause() {
                scause::Trap::Exception(scause::Exception::UserEnvCall) => {
                    use syscall::{SyscallId as Id, SyscallResult as Ret};
                    let ctx = &mut task.context.context;
                    let id: Id = ctx.a(7).into();
                    let args = [ctx.a(0), ctx.a(1), ctx.a(2), ctx.a(3), ctx.a(4), ctx.a(5)];
                    match syscall::handle(Caller { entity: task.pid, flow: 0 }, id, args) {
                        Ret::Done(ret) => match id {
                            Id::EXIT => unsafe { 
                                PIDALLOCATOR.dealloc(task.pid);
                                TASKMANAGER.del(task.pid); 
                            },
                            _ => {
                                *ctx.a_mut(0) = ret as _;
                                ctx.move_next();
                                unsafe { TASKMANAGER.add(task.pid); }
                            }
                        },
                        Ret::Unsupported(_) => {
                            log::info!("id = {id:?}");
                            unsafe {                         
                                PIDALLOCATOR.dealloc(task.pid);
                                TASKMANAGER.del(task.pid); 
                            }
                        }
                    }
                }
                e => {
                    log::error!("unsupported trap: {e:?}");
                    unsafe { 
                        PIDALLOCATOR.dealloc(task.pid);
                        TASKMANAGER.del(task.pid); 
                    }
                }
            }
        } else {
            println!("no task");
            break;
        }        
    }
    system_reset(RESET_TYPE_SHUTDOWN, RESET_REASON_NO_REASON);
    unreachable!()
}

/// Rust 异常处理函数，以异常方式关机。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("{info}");
    system_reset(RESET_TYPE_SHUTDOWN, RESET_REASON_SYSTEM_FAILURE);
    unreachable!()
}

fn kernel_space() -> AddressSpace<Sv39, Sv39Manager> {
    // 打印段位置
    extern "C" {
        fn __text();
        fn __rodata();
        fn __data();
        fn __end();
    }
    let _text = VAddr::<Sv39>::new(__text as _);
    let _rodata = VAddr::<Sv39>::new(__rodata as _);
    let _data = VAddr::<Sv39>::new(__data as _);
    let _end = VAddr::<Sv39>::new(__end as _);
    log::info!("__text ----> {:#10x}", _text.val());
    log::info!("__rodata --> {:#10x}", _rodata.val());
    log::info!("__data ----> {:#10x}", _data.val());
    log::info!("__end -----> {:#10x}", _end.val());
    println!();

    // 内核地址空间
    let mut space = AddressSpace::<Sv39, Sv39Manager>::new();
    space.map_extern(
        _text.floor().._rodata.ceil(),
        PPN::new(_text.floor().val()),
        VmFlags::build_from_str("X_RV"),
    );
    space.map_extern(
        _rodata.floor().._data.ceil(),
        PPN::new(_rodata.floor().val()),
        VmFlags::build_from_str("__RV"),
    );
    space.map_extern(
        _data.floor().._end.ceil(),
        PPN::new(_data.floor().val()),
        VmFlags::build_from_str("_WRV"),
    );
    // log::debug!("{space:?}");
    println!();
    unsafe { satp::set(satp::Mode::Sv39, 0, space.root_ppn().val()) };
    space
}


/// 各种接口库的实现。
mod impls {
    use crate::{mm::PAGE, TASKMANAGER, process::Process};
    use alloc::{alloc::handle_alloc_error};
    use core::{alloc::Layout, num::NonZeroUsize, ptr::NonNull};
    use kernel_vm::{
        page_table::{MmuMeta, Pte, Sv39, VAddr, VmFlags, PPN, VPN},
        PageManager,
    };
    use output::log;
    use syscall::*;

    #[repr(transparent)]
    pub struct Sv39Manager(NonNull<Pte<Sv39>>);

    impl Sv39Manager {
        const OWNED: VmFlags<Sv39> = unsafe { VmFlags::from_raw(1 << 8) };
    }

    impl PageManager<Sv39> for Sv39Manager {
        #[inline]
        fn new_root() -> Self {
            const SIZE: usize = 1 << Sv39::PAGE_BITS;
            unsafe {
                match PAGE.allocate(Sv39::PAGE_BITS, NonZeroUsize::new_unchecked(SIZE)) {
                    Ok((ptr, _)) => Self(ptr),
                    Err(_) => handle_alloc_error(Layout::from_size_align_unchecked(SIZE, SIZE)),
                }
            }
        }

        #[inline]
        fn root_ppn(&self) -> PPN<Sv39> {
            PPN::new(self.0.as_ptr() as usize >> Sv39::PAGE_BITS)
        }

        #[inline]
        fn root_ptr(&self) -> NonNull<Pte<Sv39>> {
            self.0
        }

        #[inline]
        fn p_to_v<T>(&self, ppn: PPN<Sv39>) -> NonNull<T> {
            unsafe { NonNull::new_unchecked(VPN::<Sv39>::new(ppn.val()).base().as_mut_ptr()) }
        }

        #[inline]
        fn v_to_p<T>(&self, ptr: NonNull<T>) -> PPN<Sv39> {
            PPN::new(VAddr::<Sv39>::new(ptr.as_ptr() as _).floor().val())
        }

        #[inline]
        fn check_owned(&self, pte: Pte<Sv39>) -> bool {
            pte.flags().contains(Self::OWNED)
        }

        fn allocate(&mut self, len: usize, flags: &mut VmFlags<Sv39>) -> NonNull<u8> {
            unsafe {
                match PAGE.allocate(
                    Sv39::PAGE_BITS,
                    NonZeroUsize::new_unchecked(len << Sv39::PAGE_BITS),
                ) {
                    Ok((ptr, size)) => {
                        assert_eq!(size, len << Sv39::PAGE_BITS);
                        *flags |= Self::OWNED;
                        ptr
                    }
                    Err(_) => handle_alloc_error(Layout::from_size_align_unchecked(
                        len << Sv39::PAGE_BITS,
                        1 << Sv39::PAGE_BITS,
                    )),
                }
            }
        }

        fn deallocate(&mut self, _pte: Pte<Sv39>, _len: usize) -> usize {
            todo!()
        }

        fn drop_root(&mut self) {
            todo!()
        }
    }

    pub struct Console;

    impl output::Console for Console {
        #[inline]
        fn put_char(&self, c: u8) {
            #[allow(deprecated)]
            sbi_rt::legacy::console_putchar(c as _);
        }
    }

    pub struct SyscallContext;

    impl IO for SyscallContext {
        #[inline]
        fn write(&self, caller: Caller, fd: usize, buf: usize, count: usize) -> isize {
            const READABLE: VmFlags<Sv39> = VmFlags::build_from_str("RV");

            if fd == 0 {
                if let Some(ptr) = unsafe { TASKMANAGER.get_task(caller.entity).unwrap() }
                    .address_space
                    .translate(VAddr::new(buf), READABLE)
                {
                    print!("{}", unsafe {
                        core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                            ptr.as_ptr(),
                            count,
                        ))
                    });
                    count as _
                } else {
                    log::error!("ptr not readable");
                    -1
                }
            } else {
                log::error!("unsupported fd: {fd}");
                -1
            }
        }
    }

    impl syscall::Process for SyscallContext {
        #[inline]
        fn exit(&self, _caller: Caller, _status: usize) -> isize {
            0
        }
        
        fn fork(&self, caller: Caller) -> isize {
            let current = unsafe { TASKMANAGER.get_task(caller.entity).unwrap() };
            let mut child_proc = Process::fork(current).unwrap();
            let pid = child_proc.pid;
            let context = &mut child_proc.context.context;
            *context.a_mut(0) = 0 as _;
            context.move_next();
            unsafe { TASKMANAGER.insert(pid, child_proc); }
            pid as isize
        }

        fn exec(&self, _caller: Caller, _path: usize, _count: usize) -> isize {
            // const READABLE: VmFlags<Sv39> = VmFlags::build_from_str("RV");
            // if let Some(ptr) = unsafe { TASKMANAGER.current().unwrap().as_ref() }
            //         .address_space
            //         .lock()
            //         .translate(VAddr::new(path as usize), READABLE) 
            // {
            //         println!("here");
            // }
            // let proc = Process::from_elf(elf);
            todo!();
            
        }

        fn wait4(&self, _caller: Caller, _pid: usize, _exit_code_ptr: usize) -> isize {
            todo!();
        }
    }

    impl Scheduling for SyscallContext {
        #[inline]
        fn sched_yield(&self, _caller: Caller) -> isize {
            0
        }
    }

    impl Clock for SyscallContext {
        #[inline]
        fn clock_gettime(&self, caller: Caller, clock_id: ClockId, tp: usize) -> isize {
            const WRITABLE: VmFlags<Sv39> = VmFlags::build_from_str("W_V");
            match clock_id {
                ClockId::CLOCK_MONOTONIC => {
                    if let Some(mut ptr) = unsafe { TASKMANAGER.get_task(caller.entity).unwrap() }
                    .address_space
                    .translate(VAddr::new(tp), WRITABLE)
                    {
                        let time = riscv::register::time::read() * 10000 / 125;
                        *unsafe { ptr.as_mut() } = TimeSpec {
                            tv_sec: time / 1_000_000_000,
                            tv_nsec: time % 1_000_000_000,
                        };
                        0
                    } else {
                        log::error!("ptr not readable");
                        -1
                    }
                }
                _ => -1,
            }
        }
    }
}