//! Locks the agent's own processes down once they have started what they
//! must (Linux). The program the user asked for is never restricted: it is
//! started before, and runs with the user's full rights, as over `ssh -X`.
//!
//! What a process that read hostile client input could still do afterwards
//! is cut to what it needs:
//!
//! - **No new privileges** (`PR_SET_NO_NEW_PRIVS`): no setuid program can
//!   lift it again.
//! - **Not dumpable, no core files**: other processes of the user cannot
//!   ptrace it or read its memory, and a crash leaves nothing on disk.
//! - **seccomp-bpf**: no program can be started (`execve`), no network
//!   socket opened (only Unix ones), and none of the kernel's riskier
//!   interfaces used (ptrace, namespaces, BPF, io_uring, modules, keyrings,
//!   `TIOCSTI` and the like).
//! - **Landlock**: files only in the session directory (and, when asked,
//!   removing the agent's own file), no TCP, no abstract Unix sockets of
//!   other processes. Best effort: kernels before 5.13 have none of it, and
//!   `--check-sandbox` says what this host enforces.

use std::collections::BTreeMap;
use std::path::Path;

use landlock::{
    ABI, Access, AccessFs, AccessNet, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// The newest Landlock ABI asked for; older kernels get what they have.
const LANDLOCK_ABI: ABI = ABI::V6;

/// What a locked-down process may still reach on disk.
#[derive(Debug, Default)]
pub struct Files<'a> {
    /// Read, write, make and remove anything beneath these.
    pub own: Vec<&'a Path>,
    /// Only remove files directly in these (the agent's own directory).
    pub remove_in: Vec<&'a Path>,
}

/// What was enforced, for the session's log and `--check-sandbox`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub no_new_privs: bool,
    pub not_dumpable: bool,
    pub seccomp: Result<(), String>,
    pub landlock: Result<String, String>,
}

impl Report {
    /// One line per measure.
    pub fn lines(&self) -> Vec<String> {
        let yes = |b: bool| if b { "on" } else { "NOT on" };
        vec![
            format!("no new privileges: {}", yes(self.no_new_privs)),
            format!("not dumpable, no core files: {}", yes(self.not_dumpable)),
            match &self.seccomp {
                Ok(()) => "seccomp: on (no exec, no network sockets, no risky syscalls)".into(),
                Err(e) => format!("seccomp: NOT on ({e})"),
            },
            match &self.landlock {
                Ok(s) => format!("Landlock: {s}"),
                Err(e) => format!("Landlock: NOT on ({e})"),
            },
        ]
    }
}

/// Locks the calling process down. Call it before the process starts
/// threads of its own: Landlock binds only the calling thread and those it
/// starts afterwards (seccomp is put on every thread).
pub fn lock_down(files: &Files<'_>) -> Report {
    let not_dumpable = set_not_dumpable();
    // Landlock first: it sets no_new_privs itself, and seccomp needs it.
    let landlock = landlock(files);
    let no_new_privs = rustix::thread::set_no_new_privs(true).is_ok();
    let seccomp = seccomp();
    Report {
        no_new_privs,
        not_dumpable,
        seccomp,
        landlock,
    }
}

fn set_not_dumpable() -> bool {
    use rustix::process::{Resource, Rlimit, setrlimit};
    let core = setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    );
    let dump =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable);
    core.is_ok() && dump.is_ok()
}

fn landlock(files: &Files<'_>) -> Result<String, String> {
    let err = |e: &dyn std::fmt::Display| e.to_string();
    let all = AccessFs::from_all(LANDLOCK_ABI);
    let own = all & !AccessFs::Execute;
    let remove = AccessFs::RemoveFile | AccessFs::ReadDir;
    let mut ruleset = Ruleset::default()
        .handle_access(all)
        .map_err(|e| err(&e))?
        .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
        .map_err(|e| err(&e))?
        .scope(Scope::AbstractUnixSocket)
        .map_err(|e| err(&e))?
        .create()
        .map_err(|e| err(&e))?;
    for dir in &files.own {
        let fd = PathFd::new(dir).map_err(|e| err(&e))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, own))
            .map_err(|e| err(&e))?;
    }
    for dir in &files.remove_in {
        // The agent's own directory may be gone or unreadable: then there
        // is nothing to remove there anyway.
        if let Ok(fd) = PathFd::new(dir) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, remove))
                .map_err(|e| err(&e))?;
        }
    }
    let status = ruleset.restrict_self().map_err(|e| err(&e))?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok("on".into()),
        RulesetStatus::PartiallyEnforced => Ok("partly on (an older kernel)".into()),
        RulesetStatus::NotEnforced => Err("this kernel has no Landlock".into()),
    }
}

/// Syscalls refused with `EPERM` whatever their arguments.
const DENIED: &[i64] = &[
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_pidfd_getfd,
    libc::SYS_kcmp,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_fsopen,
    libc::SYS_fsmount,
    libc::SYS_fsconfig,
    libc::SYS_fspick,
    libc::SYS_setns,
    libc::SYS_unshare,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_kexec_load,
    libc::SYS_reboot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_acct,
    libc::SYS_quotactl,
    libc::SYS_personality,
    libc::SYS_name_to_handle_at,
    libc::SYS_open_by_handle_at,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_adjtimex,
    libc::SYS_clock_adjtime,
    libc::SYS_sethostname,
    libc::SYS_setdomainname,
    libc::SYS_mbind,
    libc::SYS_migrate_pages,
    libc::SYS_move_pages,
    libc::SYS_lookup_dcookie,
    libc::SYS_vhangup,
];

/// Namespace flags `clone` may not carry.
const NEW_NAMESPACES: &[u64] = &[
    libc::CLONE_NEWUSER as u64,
    libc::CLONE_NEWNS as u64,
    libc::CLONE_NEWNET as u64,
    libc::CLONE_NEWPID as u64,
    libc::CLONE_NEWIPC as u64,
    libc::CLONE_NEWUTS as u64,
    libc::CLONE_NEWCGROUP as u64,
];

fn seccomp() -> Result<(), String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
    let err = |e: seccompiler::BackendError| e.to_string();
    let cond = |arg, len, op, value| SeccompCondition::new(arg, len, op, value).map_err(err);
    let rule = |c: Vec<SeccompCondition>| SeccompRule::new(c).map_err(err);

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        DENIED.iter().map(|&nr| (nr, Vec::new())).collect();
    // Only Unix sockets: no network, no netlink, no raw packets.
    rules.insert(
        libc::SYS_socket,
        vec![rule(vec![cond(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )?])?],
    );
    // No typing into a terminal, no console tricks.
    rules.insert(
        libc::SYS_ioctl,
        [libc::TIOCSTI, libc::TIOCLINUX]
            .iter()
            .map(|&req| {
                rule(vec![cond(
                    1,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Eq,
                    // c_ulong with glibc, c_int with musl.
                    #[allow(clippy::unnecessary_cast)]
                    {
                        req as u64
                    },
                )?])
            })
            .collect::<Result<_, _>>()?,
    );
    // Threads, yes; new namespaces, no.
    rules.insert(
        libc::SYS_clone,
        NEW_NAMESPACES
            .iter()
            .map(|&flag| {
                rule(vec![cond(
                    0,
                    SeccompCmpArgLen::Qword,
                    SeccompCmpOp::MaskedEq(flag),
                    flag,
                )?])
            })
            .collect::<Result<_, _>>()?,
    );
    let denied = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(err)?;
    // clone3 passes its flags in memory, out of a filter's reach: refuse it
    // as "not there", and the C library falls back to clone, checked above.
    let clone3 = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
    .map_err(err)?;
    for filter in [denied, clone3] {
        let program: BpfProgram = filter.try_into().map_err(err)?;
        seccompiler::apply_filter_all_threads(&program).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `ferostream-agent --check-sandbox`: locks itself down as a session does
/// and tries what a taken-over session would. Exit code 0 when everything
/// that should be refused was.
pub fn check(base: &Path) -> std::process::ExitCode {
    let report = lock_down(&Files {
        own: vec![base],
        remove_in: Vec::new(),
    });
    for line in report.lines() {
        println!("{line}");
    }
    let mut ok = report.seccomp.is_ok() && report.no_new_privs;
    let mut probe = |what: &str, done: bool, should: bool| {
        let word = match (done, should) {
            (true, true) => "works",
            (false, false) => "refused",
            (true, false) => "ALLOWED",
            (false, true) => "FAILED",
        };
        println!("{what}: {word}");
        ok &= done == should;
    };
    probe(
        "start a program",
        std::process::Command::new("/bin/true").status().is_ok(),
        false,
    );
    probe(
        "open a network socket",
        std::net::UdpSocket::bind("127.0.0.1:0").is_ok(),
        false,
    );
    if report.landlock.is_ok() {
        probe("list /etc", std::fs::read_dir("/etc").is_ok(), false);
    }
    let own = base.join(".check-sandbox");
    probe(
        "write in the session directory",
        std::fs::write(&own, b"").is_ok() && std::fs::remove_file(&own).is_ok(),
        true,
    );
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
