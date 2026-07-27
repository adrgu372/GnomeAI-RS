//! Linux execution policy for coding-agent commands.
//!
//! Strict internal/read-only jobs apply three layers, in this order, inside a
//! freshly exec'd helper process:
//!   1. rlimits + setsid  — resource caps and a killable process group
//!   2. Landlock          — filesystem access control (unprivileged, no userns)
//!   3. seccomp-bpf       — syscall filtering, mainly to kill network + escapes
//!
//! User-facing `normal` and `full-access` deliberately keep only the killable
//! process group and timeout/output enforcement. Their difference is approval
//! policy in the agent loop, not hidden filesystem confinement here.
//!
//! Design note: none of this runs in `pre_exec` of a multithreaded process.
//! Instead we re-exec our own binary with argv[0] = SANDBOX_ARG0; the helper is
//! single-threaded at that point, so applying the filters is safe.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub const SANDBOX_ARG0: &str = "gnomef-sandbox";
const POLICY_ENV: &str = "GNOMEF_SANDBOX_POLICY";

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Read the workspace, write nothing.
    ReadOnly,
    /// Commands run with the user's normal OS access after explicit approval.
    #[serde(alias = "workspace-write")]
    Normal,
    /// Commands run with the user's normal OS access without approval.
    #[serde(alias = "danger-full-access")]
    FullAccess,
    /// Internal isolation for generated programs and untrusted parsers.
    IsolatedWorkspaceWrite,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub cwd: PathBuf,
    /// Paths the child may write to (recursively).
    pub writable: Vec<PathBuf>,
    /// Paths the child may read (recursively).
    pub readable: Vec<PathBuf>,
    /// If false, AF_INET/AF_INET6/AF_PACKET sockets return EAFNOSUPPORT.
    pub allow_network: bool,
    /// Refuse to execute when Landlock is unavailable instead of degrading to
    /// seccomp-only isolation. Required for model-generated code and parsers
    /// that consume untrusted files.
    #[serde(default)]
    pub require_landlock: bool,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    /// Env vars forwarded to the child. Everything else is cleared.
    pub env_allowlist: Vec<String>,
    /// Env vars set explicitly for the child, regardless of our own
    /// environment. Used to tune sandboxed helpers (for example forcing a
    /// single-threaded OpenMP so `RLIMIT_NPROC` cannot stop them).
    #[serde(default)]
    pub env_extra: Vec<(String, String)>,
}

impl SandboxPolicy {
    /// Strict internal policy: can build and test, cannot reach the network or
    /// touch anything outside the workspace.
    pub fn isolated_workspace_write(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let home = dirs_home();

        let mut writable = vec![
            cwd.clone(),
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
        ];
        // Build caches. Without these, cargo/npm/pip fail in confusing ways.
        if let Some(h) = &home {
            writable.push(h.join(".cargo/registry"));
            writable.push(h.join(".cargo/git"));
            writable.push(h.join(".cache"));
            writable.push(h.join(".npm"));
        }

        let mut readable = vec![
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/etc"),
            PathBuf::from("/opt"),
            PathBuf::from("/proc"),
            PathBuf::from("/sys/devices/system/cpu"), // rustc/make read core counts
        ];
        if let Some(h) = &home {
            readable.push(h.join(".rustup"));
        }

        Self {
            mode: SandboxMode::IsolatedWorkspaceWrite,
            cwd,
            writable,
            readable,
            allow_network: false,
            require_landlock: false,
            timeout_ms: 120_000,
            max_output_bytes: 64 * 1024,
            env_allowlist: vec![
                "PATH".into(),
                "HOME".into(),
                "LANG".into(),
                "LC_ALL".into(),
                "TERM".into(),
                "TMPDIR".into(),
                "CARGO_HOME".into(),
                "RUSTUP_HOME".into(),
                "CARGO_TARGET_DIR".into(),
            ],
            env_extra: Vec::new(),
        }
    }

    pub fn read_only(cwd: impl Into<PathBuf>) -> Self {
        let mut p = Self::isolated_workspace_write(cwd);
        p.mode = SandboxMode::ReadOnly;
        p.readable.push(p.cwd.clone());
        p.writable = vec![PathBuf::from("/tmp")];
        p
    }

    pub fn normal(cwd: impl Into<PathBuf>) -> Self {
        Self::unrestricted(cwd, SandboxMode::Normal)
    }

    pub fn full_access(cwd: impl Into<PathBuf>) -> Self {
        Self::unrestricted(cwd, SandboxMode::FullAccess)
    }

    fn unrestricted(cwd: impl Into<PathBuf>, mode: SandboxMode) -> Self {
        Self {
            mode,
            cwd: cwd.into(),
            writable: Vec::new(),
            readable: Vec::new(),
            allow_network: true,
            require_landlock: false,
            timeout_ms: 120_000,
            max_output_bytes: 64 * 1024,
            env_allowlist: Vec::new(),
            env_extra: Vec::new(),
        }
    }

    fn inherits_environment(&self) -> bool {
        matches!(self.mode, SandboxMode::Normal | SandboxMode::FullAccess)
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Parent side: spawn
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub truncated: bool,
}

pub async fn spawn_sandboxed(
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
) -> Result<ExecOutput> {
    spawn_sandboxed_with_cancel(policy, program, args, &CancellationToken::new()).await
}

pub async fn spawn_sandboxed_with_cancel(
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
    cancel: &CancellationToken,
) -> Result<ExecOutput> {
    let policy_json = serde_json::to_string(policy)?;

    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg0(SANDBOX_ARG0);
    cmd.arg(program);
    cmd.args(args);

    if !policy.inherits_environment() {
        // Strict internal jobs get an allowlisted environment. Interactive
        // normal/full-access commands intentionally inherit the user's
        // environment so approved desktop/build commands behave normally.
        cmd.env_clear();
        for key in &policy.env_allowlist {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
        }
    }
    for (key, value) in &policy.env_extra {
        cmd.env(key, value);
    }
    cmd.env(POLICY_ENV, policy_json);

    cmd.current_dir(&policy.cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context("failed to spawn sandbox helper")?;
    let pid = child.id().context("child has no pid")? as i32;

    let mut stdout_pipe = child.stdout.take().context("no stdout pipe")?;
    let mut stderr_pipe = child.stderr.take().context("no stderr pipe")?;

    let cap = policy.max_output_bytes;
    let out_task = tokio::spawn(async move { read_capped(&mut stdout_pipe, cap).await });
    let err_task = tokio::spawn(async move { read_capped(&mut stderr_pipe, cap).await });

    let timeout = Duration::from_millis(policy.timeout_ms);
    let mut timed_out = false;
    let mut cancelled = false;

    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            cancelled = true;
            kill_group(pid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(250)).await;
            kill_group(pid, libc::SIGKILL);
            child.wait().await?
        }
        result = tokio::time::timeout(timeout, child.wait()) => match result {
            Ok(res) => res?,
            Err(_) => {
            timed_out = true;
            // SIGTERM the whole process group, grace period, then SIGKILL.
            // `kill_on_drop` only reaches the direct child; a `cargo build` that
            // forked twenty rustc processes would otherwise leak all of them.
            kill_group(pid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(2_000)).await;
            kill_group(pid, libc::SIGKILL);
            child.wait().await?
            }
        }
    };

    let (stdout, out_trunc) = out_task.await??;
    let (stderr, err_trunc) = err_task.await??;

    Ok(ExecOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        timed_out,
        cancelled,
        truncated: out_trunc || err_trunc,
    })
}

async fn read_capped<R>(reader: &mut R, cap: usize) -> Result<(String, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(cap.min(8192));
    let mut chunk = [0u8; 8192];
    let mut total = 0usize;
    let mut truncated = false;

    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        total += n;
        if buf.len() < cap {
            let take = n.min(cap - buf.len());
            buf.extend_from_slice(&chunk[..take]);
        } else {
            truncated = true;
            // Keep draining so the child never blocks on a full pipe.
        }
    }

    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        s.push_str(&format!(
            "\n\n[... output truncated, {} bytes total ...]",
            total
        ));
    }
    Ok((s, truncated))
}

fn kill_group(pid: i32, sig: libc::c_int) {
    // The helper called setsid(), so pgid == pid.
    unsafe {
        libc::killpg(pid, sig);
    }
}

// ---------------------------------------------------------------------------
// Child side: the helper. Call this at the very top of main().
// ---------------------------------------------------------------------------

/// If argv[0] is SANDBOX_ARG0, apply the sandbox and exec the real command.
/// Never returns in that case.
pub fn maybe_run_as_helper() -> Result<()> {
    let argv0 = std::env::args().next().unwrap_or_default();
    if argv0 != SANDBOX_ARG0 {
        return Ok(());
    }

    let policy: SandboxPolicy = serde_json::from_str(
        &std::env::var(POLICY_ENV).context("sandbox policy env var missing")?,
    )?;
    // The helper runs before the Tokio runtime starts, so no other thread can
    // concurrently inspect or modify the process environment.
    unsafe {
        std::env::remove_var(POLICY_ENV);
    }

    let mut args = std::env::args().skip(1);
    let program = args.next().context("no program to exec")?;
    let rest: Vec<String> = args.collect();

    apply_sandbox(&policy)?;

    // exec replaces this process; on success it does not return.
    let err = exec(&program, &rest);
    bail!("exec {program} failed: {err}");
}

fn apply_sandbox(policy: &SandboxPolicy) -> Result<()> {
    if matches!(policy.mode, SandboxMode::Normal | SandboxMode::FullAccess) {
        // Still set up the process group so timeouts work.
        unsafe { libc::setsid() };
        return Ok(());
    }

    unsafe { libc::setsid() };
    set_rlimits()?;
    set_no_new_privs()?;
    let landlock_enforced = apply_landlock(policy)?;
    if policy.require_landlock && !landlock_enforced {
        bail!(
            "Landlock filesystem isolation is unavailable; refusing to run \
             untrusted code"
        );
    }
    apply_seccomp(policy)?;
    Ok(())
}

// --- layer 1: rlimits ------------------------------------------------------

fn set_rlimits() -> Result<()> {
    // Deliberately NOT setting RLIMIT_AS: rustc, the JVM and anything using a
    // large virtual reservation break under it in ways that look like compiler
    // bugs. Use cgroups if you need a real memory cap.
    setrlimit(libc::RLIMIT_FSIZE, 2 * 1024 * 1024 * 1024)?; // 2 GiB per file
    setrlimit(libc::RLIMIT_NPROC, 512)?; // fork-bomb guard
    setrlimit(libc::RLIMIT_NOFILE, 4096)?;
    setrlimit(libc::RLIMIT_CORE, 0)?; // no core dumps
    Ok(())
}

fn setrlimit(resource: libc::__rlimit_resource_t, value: u64) -> Result<()> {
    let lim = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    let rc = unsafe { libc::setrlimit(resource, &lim) };
    if rc != 0 {
        bail!(
            "setrlimit({resource}) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn set_no_new_privs() -> Result<()> {
    // Required before loading a seccomp filter unprivileged, and it stops any
    // setuid binary in the sandbox from regaining privileges.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        bail!(
            "PR_SET_NO_NEW_PRIVS failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

// --- layer 2: Landlock -----------------------------------------------------

fn apply_landlock(policy: &SandboxPolicy) -> Result<bool> {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, path_beneath_rules,
    };

    // Pin to a known ABI and let the crate degrade gracefully on older kernels
    // rather than refusing to start. Landlock landed in 5.13; anything older
    // silently gets no filesystem confinement, hence the warning below.
    let abi = ABI::V3;

    let ro = AccessFs::from_read(abi);
    let rw = AccessFs::from_all(abi);

    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(rw)?
        .create()?
        .add_rules(path_beneath_rules(&policy.readable, ro))?
        .add_rules(path_beneath_rules(&policy.writable, rw))?
        // Character devices programs assume exist.
        .add_rules(path_beneath_rules(
            ["/dev/null", "/dev/zero", "/dev/urandom", "/dev/random"],
            rw,
        ))?
        .restrict_self()?;

    match status.ruleset {
        RulesetStatus::FullyEnforced => {}
        RulesetStatus::PartiallyEnforced => eprintln!(
            "gnomef-sandbox: WARNING — Landlock is only partially enforced. \
             Some filesystem rights are unavailable on this kernel."
        ),
        RulesetStatus::NotEnforced => eprintln!(
            "gnomef-sandbox: WARNING — Landlock unavailable (kernel < 5.13 or \
             disabled). Filesystem is NOT confined."
        ),
    }
    Ok(status.ruleset == RulesetStatus::FullyEnforced)
}

// --- layer 3: seccomp ------------------------------------------------------

fn apply_seccomp(policy: &SandboxPolicy) -> Result<()> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // Denylist, not allowlist. An allowlist that covers arbitrary build
    // toolchains is a maintenance sink; Landlock already carries the weight
    // for filesystem access. seccomp here is for the things Landlock cannot
    // express: network families and namespace/tracing escapes.

    if !policy.allow_network {
        // socket(domain, ...) — args are scalars, so BPF can inspect them.
        // AF_UNIX is deliberately allowed: blocking it breaks build tooling.
        // AF_NETLINK is allowed too, because glibc's getaddrinfo uses it to
        // enumerate interfaces and blocking it causes hangs instead of clean
        // errors. AF_INET being gone is enough to stop real traffic.
        let mut socket_rules = Vec::new();
        for domain in [libc::AF_INET, libc::AF_INET6, libc::AF_PACKET] {
            socket_rules.push(SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                domain as u64,
            )?])?);
        }
        rules.insert(libc::SYS_socket, socket_rules);
    }

    // Namespace and mount escapes.
    for syscall in [
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
    ] {
        rules.insert(syscall, vec![]);
    }

    // Tracing / introspection of other processes.
    for syscall in [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ] {
        rules.insert(syscall, vec![]);
    }

    // Kernel-surface syscalls with a bad CVE history.
    for syscall in [
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
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
    ] {
        rules.insert(syscall, vec![]);
    }

    // EPERM rather than SIGSYS: programs handle an errno gracefully and report
    // something useful. A killed process just looks like a crash to the model,
    // which then wastes turns debugging the wrong thing.
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                     // default
        SeccompAction::Errno(libc::EPERM as u32), // on match
        std::env::consts::ARCH.try_into()?,
    )?;

    let prog: BpfProgram = filter.try_into()?;
    seccompiler::apply_filter(&prog)?;
    Ok(())
}

// --- exec ------------------------------------------------------------------

fn exec(program: &str, args: &[String]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(program).args(args).exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_mode_names_and_legacy_aliases_deserialize() {
        assert_eq!(
            serde_json::from_str::<SandboxMode>(r#""normal""#).unwrap(),
            SandboxMode::Normal
        );
        assert_eq!(
            serde_json::from_str::<SandboxMode>(r#""workspace-write""#).unwrap(),
            SandboxMode::Normal
        );
        assert_eq!(
            serde_json::from_str::<SandboxMode>(r#""full-access""#).unwrap(),
            SandboxMode::FullAccess
        );
        assert_eq!(
            serde_json::from_str::<SandboxMode>(r#""danger-full-access""#).unwrap(),
            SandboxMode::FullAccess
        );
        assert_eq!(
            serde_json::to_string(&SandboxMode::Normal).unwrap(),
            r#""normal""#
        );
        assert_eq!(
            serde_json::to_string(&SandboxMode::FullAccess).unwrap(),
            r#""full-access""#
        );
    }

    #[test]
    fn normal_and_full_access_are_not_filesystem_sandboxes() {
        for policy in [
            SandboxPolicy::normal("/tmp"),
            SandboxPolicy::full_access("/tmp"),
        ] {
            assert!(policy.allow_network);
            assert!(policy.readable.is_empty());
            assert!(policy.writable.is_empty());
            assert!(policy.inherits_environment());
        }
    }
}
