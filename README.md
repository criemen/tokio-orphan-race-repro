# tokio orphan-queue race: killed child stays a zombie

Minimal, deterministic repro for a race in tokio's Unix process reaping
(`tokio/src/process/unix/{reap.rs,pidfd_reaper.rs,orphan.rs}`).

## Run

```sh
cargo run   # tokio 1.54.1 from crates.io -> exits 1
# with fix.patch applied to a local tokio checkout -> exits 0:
cargo run --config 'patch.crates-io.tokio.path="/path/to/tokio-checkout/tokio"'
```

Observed on macOS 27.2 (arm64), rustc 1.98.1, tokio 1.54.1 (20/20 runs):

```
$ cargo run
ps 3s after drop: 76581 Z
BUG: child 76581 is still a zombie
```

With the fix: `OK: child <pid> was reaped`.

`src/main.rs` overrides `waitpid`, which std's `Child::try_wait` calls. For the
target child it answers "still running" and waits until the runtime thread has
handled the child's SIGCHLD. All other calls go to the real `wait4`.

---

## Draft bug report

**Title:** Dropped `Child` can stay a zombie forever: race between `Reaper::drop` and `reap_orphans`

**Summary**

When a `tokio::process::Child` is dropped before it has exited (e.g. with
`kill_on_drop(true)`), `Reaper::drop` (and `PidfdReaper::drop` on Linux) does:

```rust
if let Ok(Some(_)) = self.inner_mut().try_wait() { return; } // (A)
let orphan = self.inner.take().unwrap();
self.orphan_queue.push_orphan(orphan);                       // (B)
```

`push_orphan` just pushes into the process-global orphan queue. `reap_orphans`
(run by every runtime's process driver after it parks) drains the queue only if
the shared SIGCHLD `watch::Receiver` has changed.

If the child exits between (A) and (B), and some other thread (another worker,
or another runtime) handles that SIGCHLD in between, the drain finds an empty
queue and consumes the SIGCHLD notification. The child is then pushed at (B),
and nothing drains the queue again until a *different* child exits. In a
process that spawns no more children, the child stays a zombie indefinitely.

We hit this in production tests: an rr recording showed the child staying a
zombie for 5s+, the orphan queue containing it, and the SIGCHLD receiver having
already seen the latest version. `kill_on_drop(true)` makes the window easy to
hit, because `kill()` runs immediately before `try_wait()`.

**Steps to reproduce**

Attached is a small cargo project (`orphan-race-repro`). It spawns `sleep 1000`
with `kill_on_drop(true)` and drops it from a thread other than the runtime
thread. It forces the interleaving by overriding libc `waitpid` from the binary
(no tokio changes): the `WNOHANG` check at (A) for the target child reports
"still running" and waits until the runtime has handled the child's SIGCHLD.
`cargo run` exits 1 and `ps` shows the child as `Z`.

**Expected:** the child is reaped soon after it exits.

**Actual:** the child stays a zombie until some unrelated child process exits
and generates a new SIGCHLD.

**Platforms:** reproduced on macOS (arm64), tokio 1.54.1 and current master.
The same code path exists on Linux (`PidfdReaper::drop` also falls back to
`push_orphan`).

**Suggested fix:** check the child again while holding the queue lock in
`push_orphan` (see `fix.patch`):

```rust
pub(crate) fn push_orphan(&self, mut orphan: T) where T: Wait {
    let mut queue = self.queue.lock();
    match orphan.try_wait() {
        Ok(None) => queue.push(orphan),
        Ok(Some(_)) | Err(_) => {}
    }
}
```

Any SIGCHLD that arrives after this check bumps the watch version, so the next
`reap_orphans` takes the queue lock after the push and sees the child. With
this change the repro passes. Note: some mock unit tests in `orphan.rs`
(`drain_attempts_a_single_reap_of_all_queued_orphans`, `no_reap_if_*`,
`does_*`) assert exact `try_wait` call counts and would need updating.
