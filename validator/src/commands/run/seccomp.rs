
use log::{debug, warn};

const SECCOMP_DATA_NR: u32 = 0;
const SECCOMP_DATA_ARCH: u32 = 4;
const SECCOMP_DATA_ARG0: u32 = 16;

const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_EPERM: u32 = 0x0005_0000 | (libc::EPERM as u32);

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

fn denied_syscalls() -> Vec<libc::c_long> {
    let mut denied = vec![

        libc::SYS_perf_event_open,
        libc::SYS_ptrace,
        libc::SYS_process_vm_writev,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_setns,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_reboot,
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_keyctl,
        libc::SYS_open_by_handle_at,
    ];
    #[cfg(target_arch = "x86_64")]
    {
        denied.push(libc::SYS_chroot);
        denied.push(libc::SYS_settimeofday);
        denied.push(libc::SYS_kexec_file_load);
    }
    denied
}

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

const BPF_MAP_UPDATE_ELEM: u32 = 2;

fn build_filter(denied: &[libc::c_long]) -> Vec<libc::sock_filter> {
    let n = denied.len();
    let allow_idx = 7 + n;
    let bpf_idx = 8 + n;
    let deny_idx = 11 + n;
    let to = |from: usize, target: usize| -> u8 {
        u8::try_from(target - from - 1).expect("seccomp jump offset fits in a byte")
    };

    let ld_abs = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    let jeq = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    let ret = (libc::BPF_RET | libc::BPF_K) as u16;

    let mut prog = Vec::with_capacity(deny_idx + 1);
    prog.push(stmt(ld_abs, SECCOMP_DATA_ARCH));
    prog.push(jump(jeq, AUDIT_ARCH, 0, to(1, deny_idx)));
    prog.push(stmt(ld_abs, SECCOMP_DATA_NR));
    for (i, nr) in denied.iter().enumerate() {
        let at = 3 + i;
        prog.push(jump(jeq, *nr as u32, to(at, deny_idx), 0));
    }
    prog.push(jump(jeq, libc::SYS_bpf as u32, to(3 + n, bpf_idx), 0));
    prog.push(jump(jeq, libc::SYS_socket as u32, 0, to(4 + n, allow_idx)));
    prog.push(stmt(ld_abs, SECCOMP_DATA_ARG0));
    prog.push(jump(jeq, libc::AF_XDP as u32, to(6 + n, deny_idx), 0));
    prog.push(stmt(ret, SECCOMP_RET_ALLOW));
    prog.push(stmt(ld_abs, SECCOMP_DATA_ARG0));
    prog.push(jump(jeq, BPF_MAP_UPDATE_ELEM, 0, to(9 + n, deny_idx)));
    prog.push(stmt(ret, SECCOMP_RET_ALLOW));
    prog.push(stmt(ret, SECCOMP_RET_EPERM));
    debug_assert_eq!(prog.len(), deny_idx + 1);
    prog
}

pub fn install_lockdown() {
    let denied = denied_syscalls();
    let prog = build_filter(&denied);
    let fprog = libc::sock_fprog {
        len: u16::try_from(prog.len()).expect("seccomp program length fits in a u16"),
        filter: prog.as_ptr() as *mut libc::sock_filter,
    };

    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        warn!(
            "could not set no-new-privs ({}); skipping the post-setup syscall lockdown",
            std::io::Error::last_os_error()
        );
        return;
    }

    let res = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_TSYNC,
            &fprog as *const libc::sock_fprog,
        )
    };
    if res != 0 {
        warn!(
            "could not install the post-setup syscall lockdown ({}); continuing without it",
            std::io::Error::last_os_error()
        );
        return;
    }
    debug!(
        "post-setup syscall lockdown active: {} syscalls and AF_XDP socket creation now return \
         EPERM process-wide",
        denied.len()
    );
}

