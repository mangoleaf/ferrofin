//! Time-limited, bounded probes of the mounts `GetDrives` lists.
//!
//! `DriveInfo.TotalSize` is a `statvfs`, and on a hard-mounted NFS or CIFS
//! export whose server is gone a `statvfs` sleeps uninterruptibly until the
//! server returns: no timeout can cancel it and no signal can wake it. Upstream
//! pays with a thread of a pool that grows. Here each mount is asked on a thread
//! of its own, the caller waits only so long, and a mount that does not answer
//! in time is left out of the list.
//!
//! The thread of a hung mount cannot be stopped, so what stays bounded is how
//! many there are: **one per mount**, however often the drives are listed. While
//! a probe is out, a later listing joins it instead of starting another; once
//! it has outlived the timeout, that listing does not wait for it at all. The
//! probe clears itself when the mount finally answers, and the next listing
//! asks again, which is how a mount that came back returns to the list.
//!
//! The probe threads are plain OS threads rather than the blocking pool's, so a
//! hung mount never takes a thread other blocking work is waiting for.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long a mount gets to answer before a listing goes on without it. Local
/// mounts answer in microseconds; a network mount that needs longer than this
/// is left out of that listing only. A candidate setting (sensible range 1–5 s),
/// recorded with the owner on gl-26.
pub(crate) const DRIVE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// One question out to a mount, shared by every listing waiting on it.
struct Probe {
    started: Instant,
    /// What the mount answered; `None` until it does.
    answer: Mutex<Option<bool>>,
    answered: Condvar,
    /// Whether a listing has already logged that this probe outlived the
    /// timeout: a hang is worth one warning, not one per listing.
    reported: AtomicBool,
}

impl Probe {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            answer: Mutex::new(None),
            answered: Condvar::new(),
            reported: AtomicBool::new(false),
        }
    }

    fn answer(&self) -> MutexGuard<'_, Option<bool>> {
        self.answer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn finish(&self, answer: bool) {
        *self.answer() = Some(answer);
        self.answered.notify_all();
    }

    /// The mount's answer, waiting until `timeout` after the probe started:
    /// not at all for a probe that has already outlived it.
    fn wait(&self, timeout: Duration) -> Option<bool> {
        let left = (self.started + timeout).saturating_duration_since(Instant::now());
        let (answer, _) = self
            .answered
            .wait_timeout_while(self.answer(), left, |answer| answer.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        *answer
    }
}

/// The probes out to the mounts, by mount point.
pub(crate) struct DriveProbes {
    timeout: Duration,
    in_flight: Arc<Mutex<HashMap<String, Arc<Probe>>>>,
}

impl DriveProbes {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            in_flight: Arc::default(),
        }
    }

    /// Whether `ask` holds for each of `mounts`, in order; a mount that does
    /// not answer in time counts as `false`. All are asked at once, so the
    /// call takes at most one timeout however many mounts hang.
    pub(crate) fn answers<F>(&self, mounts: &[&str], ask: &F) -> Vec<bool>
    where
        F: Fn(&str) -> bool + Clone + Send + 'static,
    {
        let probes: Vec<Option<Arc<Probe>>> = mounts
            .iter()
            .map(|mount| self.start_or_join(mount, ask))
            .collect();
        mounts
            .iter()
            .zip(probes)
            .map(|(mount, probe)| {
                probe.is_some_and(|probe| {
                    probe.wait(self.timeout).unwrap_or_else(|| {
                        report_unanswered(mount, &probe, self.timeout);
                        false
                    })
                })
            })
            .collect()
    }

    /// The probe out to `mount`, starting one if there is none; `None` when
    /// the system would not give it a thread.
    fn start_or_join<F>(&self, mount: &str, ask: &F) -> Option<Arc<Probe>>
    where
        F: Fn(&str) -> bool + Clone + Send + 'static,
    {
        let mut in_flight = lock(&self.in_flight);
        if let Some(probe) = in_flight.get(mount) {
            return Some(Arc::clone(probe));
        }
        let probe = Arc::new(Probe::new());
        let spawned = {
            let (probe, in_flight, ask) =
                (Arc::clone(&probe), Arc::clone(&self.in_flight), ask.clone());
            let mount = mount.to_owned();
            std::thread::Builder::new()
                .name("drive-probe".to_owned())
                .spawn(move || {
                    // A probe that panicked must not stay out for good: the
                    // panic hook has told of it, and the mount is a no.
                    let answer = catch_unwind(AssertUnwindSafe(|| ask(&mount))).unwrap_or(false);
                    lock(&in_flight).remove(&mount);
                    if probe.reported.load(Ordering::Relaxed) {
                        tracing::info!(path = mount, "a mount that had stopped answering answered");
                    }
                    probe.finish(answer);
                })
        };
        match spawned {
            Ok(_) => {
                in_flight.insert(mount.to_owned(), Arc::clone(&probe));
                Some(probe)
            }
            Err(error) => {
                tracing::warn!(path = mount, %error, "could not start a thread to probe a mount; leaving it out of the drives");
                None
            }
        }
    }
}

fn lock(
    in_flight: &Mutex<HashMap<String, Arc<Probe>>>,
) -> MutexGuard<'_, HashMap<String, Arc<Probe>>> {
    in_flight.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Logs that `mount` did not answer: a warning the first time, a debug line for
/// each listing after, so that a client polling the drives during a hang cannot
/// fill the log.
fn report_unanswered(mount: &str, probe: &Probe, timeout: Duration) {
    if probe.reported.swap(true, Ordering::Relaxed) {
        tracing::debug!(
            path = mount,
            "still no answer from a mount; left out of the drives"
        );
    } else {
        tracing::warn!(
            path = mount,
            timeout_ms = timeout.as_millis(),
            "a mount did not answer; leaving it out of the drives until it does"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, Condvar};

    use super::*;

    /// A mount that hangs until [`Gate::open`], counting who asked it.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
        asked: AtomicUsize,
    }

    impl Gate {
        fn wait(&self) {
            self.asked.fetch_add(1, Ordering::SeqCst);
            let open = self.open.lock().unwrap();
            drop(self.opened.wait_while(open, |open| !*open).unwrap());
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.opened.notify_all();
        }
    }

    const NAS: &str = "/mnt/nas";
    const DISK: &str = "/mnt/disk";
    /// A timeout for a test in which a mount must answer in time: its thread
    /// has to start and run, which on a loaded runner is not instant.
    const HEALTHY_MOUNT_ROOM: Duration = Duration::from_secs(1);

    fn wait_for_no_probes(probes: &DriveProbes) {
        let give_up = Instant::now() + Duration::from_secs(30);
        while !lock(&probes.in_flight).is_empty() {
            assert!(Instant::now() < give_up, "the probe never cleared itself");
            std::thread::yield_now();
        }
    }

    fn hanging_nas(gate: &Arc<Gate>) -> impl Fn(&str) -> bool + Clone + Send + 'static {
        let gate = Arc::clone(gate);
        move |mount| {
            if mount == NAS {
                gate.wait();
            }
            true
        }
    }

    #[test]
    fn a_mount_that_does_not_answer_is_left_out_while_the_others_are_listed() {
        let gate = Arc::new(Gate::default());
        let probes = DriveProbes::new(HEALTHY_MOUNT_ROOM);
        let answers = probes.answers(&[DISK, NAS, "/"], &hanging_nas(&gate));
        assert_eq!(
            answers,
            [true, false, true],
            "the order of the mounts stays"
        );
        gate.open();
    }

    /// A false answer is not a timeout: `is_dir` or `TotalSize != 0` said no.
    #[test]
    fn a_mount_that_answers_no_is_a_no() {
        let probes = DriveProbes::new(Duration::from_secs(30));
        let answers = probes.answers(&[DISK, NAS], &|mount: &str| mount == DISK);
        assert_eq!(answers, [true, false]);
    }

    /// The whole point: polling the drives during a hang costs one thread for
    /// the mount, not one per listing, and only the first listing waits.
    #[test]
    fn repeated_listings_during_a_hang_start_no_more_threads_and_do_not_wait() {
        let gate = Arc::new(Gate::default());
        let timeout = Duration::from_millis(50);
        let probes = DriveProbes::new(timeout);
        let ask = hanging_nas(&gate);

        let started = Instant::now();
        assert_eq!(probes.answers(&[NAS], &ask), [false]);
        assert!(started.elapsed() >= timeout, "the first listing waited");

        let repeats = Instant::now();
        for _ in 0..200 {
            assert_eq!(probes.answers(&[NAS], &ask), [false]);
        }
        // Each waiting a timeout would take 200 of them.
        assert!(
            repeats.elapsed() < timeout * 50,
            "200 listings of a known-hung mount took {:?}",
            repeats.elapsed()
        );
        assert_eq!(gate.asked.load(Ordering::SeqCst), 1, "one thread, ever");
        assert_eq!(lock(&probes.in_flight).len(), 1);
        gate.open();
    }

    /// Listings that overlap share the one probe too.
    #[test]
    fn concurrent_listings_share_one_probe() {
        let gate = Arc::new(Gate::default());
        let probes = DriveProbes::new(Duration::from_millis(100));
        let ask = hanging_nas(&gate);
        let all_ready = Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    all_ready.wait();
                    assert_eq!(probes.answers(&[NAS], &ask), [false]);
                });
            }
        });
        assert_eq!(gate.asked.load(Ordering::SeqCst), 1);
        gate.open();
    }

    /// A mount listed twice (one mounted over another) is asked once.
    #[test]
    fn a_mount_point_listed_twice_is_asked_once() {
        let gate = Arc::new(Gate::default());
        let probes = DriveProbes::new(Duration::from_millis(30));
        let answers = probes.answers(&[NAS, NAS], &hanging_nas(&gate));
        assert_eq!(answers, [false, false]);
        assert_eq!(gate.asked.load(Ordering::SeqCst), 1);
        gate.open();
    }

    /// A mount that came back returns to the list, and a new hang is noticed.
    #[test]
    fn a_mount_returns_to_the_list_once_it_answers() {
        let gate = Arc::new(Gate::default());
        let probes = DriveProbes::new(HEALTHY_MOUNT_ROOM);
        let ask = hanging_nas(&gate);
        assert_eq!(probes.answers(&[NAS], &ask), [false]);

        gate.open();
        wait_for_no_probes(&probes);

        assert_eq!(probes.answers(&[NAS], &ask), [true]);
        assert_eq!(gate.asked.load(Ordering::SeqCst), 2, "asked afresh");
    }

    /// A probe that panics counts as a no and clears itself, so the mount is
    /// asked again.
    #[test]
    fn a_probe_that_panics_is_a_no_and_is_asked_again() {
        let probes = DriveProbes::new(Duration::from_secs(30));
        assert_eq!(probes.answers(&[DISK], &|_: &str| panic!("boom")), [false]);
        wait_for_no_probes(&probes);
        assert_eq!(probes.answers(&[DISK], &|_: &str| true), [true]);
    }

    /// A probe that finishes inside the timeout leaves nothing behind.
    #[test]
    fn a_healthy_listing_leaves_no_probe_behind() {
        let probes = DriveProbes::new(Duration::from_secs(30));
        assert_eq!(probes.answers(&[DISK], &|_: &str| true), [true]);
        wait_for_no_probes(&probes);
    }
}
