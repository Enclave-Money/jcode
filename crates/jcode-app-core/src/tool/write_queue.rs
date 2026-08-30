//! Per-repo write serialisation.
//!
//! Several teammates prompt agents against one shared checkout. Every write to
//! that checkout goes through an agent tool, which is what makes serialising
//! here viable: nobody is typing into an editor out of band, so there is no
//! writer to miss.
//!
//! The unit is the **repo**, not the file. Two agents editing different files
//! in one repo still interleave with `git` operations, build outputs and each
//! other's reads, and a per-file lock would not stop the resulting mess. One
//! queue per repo is the granularity the design calls for.
//!
//! The lock must wrap the whole read-modify-write, not just the write syscall:
//! `edit` reads a file, computes a replacement, then writes it back, and two
//! interleaved edits to one file silently lose the first.
//!
//! ## What this does not cover
//!
//! The `bash` tool can write anything, and a shell command's writes do not pass
//! through here. Serialisation therefore covers agent file edits, not every
//! possible mutation. Closing that would mean routing shell writes through the
//! queue too, which is not possible without intercepting the child process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// One repo's queue: the lock itself, how many callers are waiting on it, and
/// who holds it right now (for "queued behind Ana" rather than an unexplained
/// stall).
pub struct RepoQueue {
    lock: tokio::sync::Mutex<()>,
    waiting: AtomicUsize,
    holder: Mutex<Option<String>>,
}

impl RepoQueue {
    fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            waiting: AtomicUsize::new(0),
            holder: Mutex::new(None),
        }
    }
}

static REPO_WRITE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<RepoQueue>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn queue_for(key: &Path) -> Arc<RepoQueue> {
    let mut map = REPO_WRITE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(key.to_path_buf())
        .or_insert_with(|| Arc::new(RepoQueue::new()))
        .clone()
}

/// The queue key for a path: the nearest ancestor holding a `.git`, else the
/// file's own directory.
///
/// Falling back to the directory rather than to one global lock matters: a
/// single global queue would serialise unrelated repos against each other, and
/// on a team server that turns independent work into a traffic jam.
pub fn repo_key(path: &Path) -> PathBuf {
    let start = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    let mut cursor = start;
    while let Some(dir) = cursor {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        cursor = dir.parent();
    }
    start.unwrap_or(path).to_path_buf()
}

/// How many callers are waiting on this repo's queue, and who holds it.
///
/// The count includes the current holder, so 0 means idle and 1 means "someone
/// is writing, nobody is queued".
pub fn queue_status(key: &Path) -> (usize, Option<String>) {
    let map = REPO_WRITE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    match map.get(key) {
        Some(queue) => {
            let holder = queue.holder.lock().unwrap_or_else(|e| e.into_inner()).clone();
            (queue.waiting.load(Ordering::SeqCst), holder)
        }
        None => (0, None),
    }
}

/// Run `body` holding this path's repo write lock.
///
/// `actor` names whoever is writing, so a teammate waiting behind this one can
/// be told what they are waiting for. `on_wait` is called with the current
/// holder when the lock is not immediately free, which is the hook the session
/// layer uses to push a "queued behind X" event rather than letting the client
/// sit on an unexplained stall.
pub async fn with_repo_write_lock<T, F, Fut>(
    path: &Path,
    actor: Option<&str>,
    on_wait: impl FnOnce(usize, Option<String>),
    body: F,
) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let key = repo_key(path);
    let queue = queue_for(&key);

    // Count this caller before attempting the lock so a waiter is visible to
    // `queue_status` for the whole time it is actually waiting.
    queue.waiting.fetch_add(1, Ordering::SeqCst);

    // Report the wait only when there is a genuine one. try_lock keeps the
    // common uncontended path free of spurious "you are queued" events.
    let guard = match queue.lock.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            let holder = queue.holder.lock().unwrap_or_else(|e| e.into_inner()).clone();
            on_wait(queue.waiting.load(Ordering::SeqCst), holder);
            queue.lock.lock().await
        }
    };

    if let Some(actor) = actor {
        *queue.holder.lock().unwrap_or_else(|e| e.into_inner()) = Some(actor.to_string());
    }

    let result = body().await;

    // Clear the holder before releasing, so the next waiter never reads a stale
    // name, and decrement whether or not `body` panicked its way out.
    *queue.holder.lock().unwrap_or_else(|e| e.into_inner()) = None;
    drop(guard);
    queue.waiting.fetch_sub(1, Ordering::SeqCst);

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn repo_key_finds_the_git_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        let file = root.join("src/deep/file.rs");
        std::fs::write(&file, "x").unwrap();

        assert_eq!(repo_key(&file), root.to_path_buf());
    }

    /// Two checkouts must not share a queue, or independent teams block each
    /// other on a shared box.
    #[test]
    fn separate_repos_get_separate_keys() {
        let temp = tempfile::TempDir::new().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::create_dir_all(b.join(".git")).unwrap();
        std::fs::write(a.join("f"), "x").unwrap();
        std::fs::write(b.join("f"), "x").unwrap();

        assert_ne!(repo_key(&a.join("f")), repo_key(&b.join("f")));
    }

    #[test]
    fn a_path_outside_any_repo_falls_back_to_its_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("loose.txt");
        std::fs::write(&file, "x").unwrap();
        // Not one global key: unrelated directories stay independent.
        assert_eq!(repo_key(&file), temp.path().to_path_buf());
    }

    /// The point of the whole module: an interleaved read-modify-write must not
    /// lose an update. Without the lock the second writer reads the original
    /// content and overwrites the first writer's change.
    #[tokio::test]
    async fn concurrent_writers_do_not_lose_an_update() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let file = root.join("counter.txt");
        std::fs::write(&file, "0").unwrap();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let file = file.clone();
            handles.push(tokio::spawn(async move {
                with_repo_write_lock(&file, None, |_, _| {}, || async {
                    let current: u32 = tokio::fs::read_to_string(&file)
                        .await
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    // Yield between read and write: without serialisation this
                    // is where the lost update happens.
                    tokio::task::yield_now().await;
                    tokio::fs::write(&file, (current + 1).to_string()).await.unwrap();
                })
                .await
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let final_value = std::fs::read_to_string(&file).unwrap();
        assert_eq!(final_value, "8", "every increment must survive");
    }

    #[tokio::test]
    async fn a_waiter_is_told_who_holds_the_queue() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let file = root.join("f.txt");
        std::fs::write(&file, "x").unwrap();

        let release = Arc::new(tokio::sync::Notify::new());
        let held = Arc::new(tokio::sync::Notify::new());

        let holder = {
            let (file, release, held) = (file.clone(), release.clone(), held.clone());
            tokio::spawn(async move {
                with_repo_write_lock(&file, Some("ana"), |_, _| {}, || async {
                    held.notify_one();
                    release.notified().await;
                })
                .await
            })
        };

        held.notified().await;

        let saw_wait = Arc::new(AtomicBool::new(false));
        let seen_holder = Arc::new(Mutex::new(None));
        {
            let (saw_wait, seen_holder) = (saw_wait.clone(), seen_holder.clone());
            let file = file.clone();
            let waiter = tokio::spawn(async move {
                with_repo_write_lock(
                    &file,
                    Some("ben"),
                    |depth, who| {
                        saw_wait.store(true, Ordering::SeqCst);
                        *seen_holder.lock().unwrap() = who.map(|w| (w, depth));
                    },
                    || async {},
                )
                .await
            });
            // Give the waiter a moment to reach the contended path.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            release.notify_one();
            waiter.await.unwrap();
        }
        holder.await.unwrap();

        assert!(saw_wait.load(Ordering::SeqCst), "the waiter must be told it is queued");
        let seen = seen_holder.lock().unwrap().clone();
        let (who, depth) = seen.expect("a holder name must be reported");
        assert_eq!(who, "ana");
        assert!(depth >= 2, "depth counts the holder plus the waiter, got {depth}");
    }

    #[tokio::test]
    async fn an_uncontended_write_reports_no_wait() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        let file = temp.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();

        let waited = Arc::new(AtomicBool::new(false));
        let flag = waited.clone();
        with_repo_write_lock(&file, Some("solo"), |_, _| flag.store(true, Ordering::SeqCst), || async {})
            .await;

        assert!(!waited.load(Ordering::SeqCst), "a free queue must not report a wait");
    }

    #[tokio::test]
    async fn the_queue_is_idle_again_after_the_write() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        let file = temp.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();

        with_repo_write_lock(&file, Some("solo"), |_, _| {}, || async {}).await;

        let (depth, holder) = queue_status(&repo_key(&file));
        assert_eq!(depth, 0, "the waiter count must return to zero");
        assert_eq!(holder, None, "a released queue must not keep a stale holder");
    }
}
