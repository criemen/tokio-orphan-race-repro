//! tokio leaves a dropped child as a zombie if it exits between `try_wait()`
//! and `push_orphan()` in `Reaper::drop` while another thread drains the
//! (still empty) orphan queue on its SIGCHLD.
//!
//! To force that ordering, this binary overrides `waitpid`, which
//! `std::process::Child::try_wait` calls (std is linked statically, so its call
//! resolves to ours). For the target child, the check in `Reaper::drop` reports
//! "still running" and waits until the runtime has handled the child's SIGCHLD.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering::SeqCst};
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};

static FAKE_RUNNING_PID: AtomicI32 = AtomicI32::new(0);
static SIGCHLDS: AtomicUsize = AtomicUsize::new(0);

#[no_mangle]
pub unsafe extern "C" fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32 {
    if pid > 0 && FAKE_RUNNING_PID.compare_exchange(pid, 0, SeqCst, SeqCst).is_ok() {
        // The process driver runs `reap_orphans()` before it polls tasks, so
        // once the listener task has counted this SIGCHLD, the empty queue has
        // already been drained and the notification consumed.
        wait_for_sigchlds(2);
        return 0; // "still running": tokio now pushes the child to the queue
    }
    libc::wait4(pid, status, options, std::ptr::null_mut())
}

fn wait_for_sigchlds(n: usize) {
    while SIGCHLDS.load(SeqCst) < n {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn spawn_sleep(kill_on_drop: bool) -> (tokio::process::Child, i32) {
    let child = tokio::process::Command::new("sleep")
        .arg("1000")
        .kill_on_drop(kill_on_drop)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap() as i32;
    (child, pid)
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();
    let _enter = handle.enter();

    // Runtime thread: handles SIGCHLDs (and drains the orphan queue) and counts them.
    let mut sigchld = signal(SignalKind::child()).unwrap();
    std::thread::spawn(move || {
        rt.block_on(async {
            loop {
                sigchld.recv().await;
                SIGCHLDS.fetch_add(1, SeqCst);
            }
        })
    });

    // Warm-up: tokio creates the orphan queue's SIGCHLD listener lazily, the
    // first time the queue is non-empty. Orphan a running child, then kill it.
    let (warmup, warmup_pid) = spawn_sleep(false);
    drop(warmup);
    unsafe { libc::kill(warmup_pid, libc::SIGKILL) };
    wait_for_sigchlds(1);

    // The race: SIGKILL, try_wait() says "running", [exit, SIGCHLD, drain], push.
    let (child, pid) = spawn_sleep(true);
    FAKE_RUNNING_PID.store(pid, SeqCst);
    drop(child);

    // No other child activity. Running `ps` (a new child) unsticks it afterwards.
    std::thread::sleep(Duration::from_secs(3));
    let ps = std::process::Command::new("ps")
        .args(["-o", "pid=,stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let ps = String::from_utf8_lossy(&ps.stdout);
    println!("ps 3s after drop: {}", ps.trim());
    if ps.contains('Z') {
        println!("BUG: child {pid} is still a zombie");
        std::process::exit(1);
    }
    println!("OK: child {pid} was reaped");
}
