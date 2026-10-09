//! A shared ceiling for folder workers and speculative scan probes.
//!
//! The ordered foreground item (or its refresh lane) owns one slot. Extra
//! folder workers and probes acquire the remaining slots without blocking:
//! nested folder work runs inline when saturated, avoiding recursive deadlocks.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Upstream's nonpositive default leaves three processors for other work.
fn effective_limit(setting: i32, processors: usize) -> usize {
    usize::try_from(setting)
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or_else(|| processors.saturating_sub(3).max(1))
}

pub(super) struct Budget {
    read: Box<dyn Fn() -> i32 + Send + Sync>,
    extra_workers: AtomicUsize,
    processors: usize,
}

impl Budget {
    pub(super) fn new(read: impl Fn() -> i32 + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            read: Box::new(read),
            extra_workers: AtomicUsize::new(0),
            processors: usize::try_from(ferrofin_db::database::usable_cores()).unwrap_or(1),
        })
    }

    /// Existing workers finish after a reduction; new work observes the limit.
    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<Permit> {
        let extras = effective_limit((self.read)(), self.processors) - 1;
        self.extra_workers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < extras).then_some(active + 1)
            })
            .ok()
            .map(|_| Permit(Arc::clone(self)))
    }
}

pub(super) struct Permit(Arc<Budget>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.extra_workers.fetch_sub(1, Ordering::Release);
    }
}

/// Only Send data crosses into a worker; each constructs its own !Sync naming rules.
#[derive(Clone)]
struct Seed<'a> {
    scope: super::PlanScope<'a>,
    locations: std::collections::HashSet<String>,
    date_added: super::DateAdded,
    direct_file_counts: std::collections::HashMap<String, usize>,
    owner_paths: std::collections::HashMap<uuid::Uuid, String>,
    budget: Option<Arc<Budget>>,
    cancel: Option<super::ScanCancel>,
}

impl<'a> Seed<'a> {
    fn of(ctx: &super::PlanCtx<'a>) -> Self {
        Self {
            scope: ctx.scope,
            locations: ctx.locations.clone(),
            date_added: ctx.date_added,
            direct_file_counts: ctx.direct_file_counts.borrow().clone(),
            owner_paths: ctx.owner_paths.borrow().clone(),
            budget: ctx.fanout.clone(),
            cancel: ctx.cancel.clone(),
        }
    }

    fn context<'b>(self, naming: &'b super::NamingOptions) -> super::PlanCtx<'b>
    where
        'a: 'b,
    {
        let mut ctx = super::PlanCtx::new(naming, self.scope).with_date_added(self.date_added);
        ctx.locations = self.locations;
        *ctx.direct_file_counts.borrow_mut() = self.direct_file_counts;
        *ctx.owner_paths.borrow_mut() = self.owner_paths;
        ctx.fanout = self.budget;
        ctx.cancel = self.cancel;
        ctx
    }
}

struct Effects {
    excluded: Vec<String>,
    unlisted: Vec<String>,
    inaccessible: Vec<String>,
    owners: std::collections::HashMap<uuid::Uuid, String>,
    counts: std::collections::HashMap<String, usize>,
}

impl Effects {
    fn of(ctx: super::PlanCtx<'_>) -> Self {
        Self {
            excluded: ctx.excluded.into_inner(),
            unlisted: ctx.unlisted.into_inner(),
            inaccessible: ctx.inaccessible.into_inner(),
            owners: ctx.owner_paths.into_inner(),
            counts: ctx.direct_file_counts.into_inner(),
        }
    }

    fn merge(self, ctx: &super::PlanCtx<'_>) {
        ctx.excluded.borrow_mut().extend(self.excluded);
        ctx.unlisted.borrow_mut().extend(self.unlisted);
        ctx.inaccessible.borrow_mut().extend(self.inaccessible);
        ctx.owner_paths.borrow_mut().extend(self.owners);
        ctx.direct_file_counts.borrow_mut().extend(self.counts);
    }
}

fn run_chunk<T, R: Default>(
    entries: &[T],
    ctx: &super::PlanCtx<'_>,
    run: &impl Fn(&T, &super::PlanCtx<'_>) -> R,
) -> Vec<R> {
    entries
        .iter()
        .map(|entry| {
            if ctx
                .cancel
                .as_ref()
                .is_some_and(super::ScanCancel::is_cancelled)
            {
                R::default()
            } else {
                run(entry, ctx)
            }
        })
        .collect()
}

/// Resolve independent children in input order, joining every worker before return.
/// Nested work falls back inline under the parent's existing slot.
pub(super) fn map<T: Sync, R: Send + Default>(
    entries: &[T],
    ctx: &super::PlanCtx<'_>,
    run: impl Fn(&T, &super::PlanCtx<'_>) -> R + Sync,
) -> Vec<R> {
    let mut permits = Vec::new();
    if let Some(budget) = &ctx.fanout {
        for _ in 1..entries.len() {
            let Some(permit) = budget.try_acquire() else {
                break;
            };
            permits.push(permit);
        }
    }
    if permits.is_empty() {
        return run_chunk(entries, ctx, &run);
    }
    let width = entries.len().div_ceil(permits.len() + 1);
    let chunks: Vec<_> = entries.chunks(width).collect();
    let seed = Seed::of(ctx);
    std::thread::scope(|scope| {
        let mut results: Vec<Option<Vec<R>>> = (0..chunks.len()).map(|_| None).collect();
        let mut handles = Vec::new();
        let mut permits = permits.into_iter();
        let mut can_spawn = true;
        for (index, chunk) in chunks.iter().enumerate().skip(1) {
            let permit = permits.next();
            if can_spawn {
                let seed = seed.clone();
                let run = &run;
                let worker = std::thread::Builder::new()
                    .name("library-folder".into())
                    .spawn_scoped(scope, move || {
                        let _permit = permit;
                        let naming = super::NamingOptions::new();
                        let worker_ctx = seed.context(&naming);
                        let result = run_chunk(chunk, &worker_ctx, run);
                        (result, Effects::of(worker_ctx))
                    });
                match worker {
                    Ok(handle) => {
                        handles.push((index, handle));
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(%error, "folder worker unavailable; resolving remaining children inline");
                        can_spawn = false;
                    }
                }
            } else {
                drop(permit);
            }
            results[index] = Some(run_chunk(chunk, ctx, &run));
        }
        drop(permits);
        results[0] = Some(run_chunk(chunks[0], ctx, &run));
        for (index, handle) in handles {
            let (rows, effects) = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            effects.merge(ctx);
            results[index] = Some(rows);
        }
        results.into_iter().flatten().flatten().collect()
    })
}

pub(super) fn directory_plans(
    entries: &[super::FileSystemEntryInfo],
    ctx: &super::PlanCtx<'_>,
    run: impl Fn(&str, &super::PlanCtx<'_>, &mut Vec<super::Planned>) + Sync,
) -> Vec<Vec<super::Planned>> {
    let dirs: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry.type_ == super::FileSystemEntryType::Directory && ctx.scope.visits(&entry.path)
        })
        .collect();
    map(&dirs, ctx, |entry, ctx| {
        let mut rows = Vec::new();
        run(&entry.path, ctx, &mut rows);
        rows
    })
}

pub(super) fn directories(
    entries: &[super::FileSystemEntryInfo],
    ctx: &super::PlanCtx<'_>,
    out: &mut Vec<super::Planned>,
    run: impl Fn(&str, &super::PlanCtx<'_>, &mut Vec<super::Planned>) + Sync,
) {
    out.extend(directory_plans(entries, ctx, run).into_iter().flatten());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;

    #[test]
    fn upstream_fanout_defaults_and_explicit_limits() {
        for (setting, processors, expected) in [
            (-1, 1, 1),
            (0, 2, 1),
            (0, 3, 1),
            (0, 4, 1),
            (0, 8, 5),
            (1, 8, 1),
            (2, 1, 2),
            (20, 4, 20),
        ] {
            assert_eq!(effective_limit(setting, processors), expected);
        }
    }

    #[test]
    fn nested_work_does_not_wait_and_live_reductions_drain_existing_slots() {
        let setting = Arc::new(AtomicI32::new(3));
        let source = Arc::clone(&setting);
        let budget = Budget::new(move || source.load(Ordering::Relaxed));
        let first = budget.try_acquire().expect("second worker");
        let second = budget.try_acquire().expect("third worker");
        assert!(budget.try_acquire().is_none());
        setting.store(1, Ordering::Relaxed);
        drop(first);
        assert!(budget.try_acquire().is_none());
        drop(second);
        assert!(budget.try_acquire().is_none());
        setting.store(2, Ordering::Relaxed);
        assert!(budget.try_acquire().is_some());
        assert_eq!(budget.extra_workers.load(Ordering::Acquire), 0);
    }

    #[test]
    fn nested_folder_work_shares_live_budget_and_keeps_order_and_effects() {
        let setting = Arc::new(AtomicI32::new(1));
        let source = Arc::clone(&setting);
        let budget = Budget::new(move || source.load(Ordering::Acquire));
        let naming = super::super::NamingOptions::new();
        let mut ctx = super::super::PlanCtx::new(&naming, super::super::PlanScope::ALL);
        ctx.fanout = Some(Arc::clone(&budget));
        let owner = uuid::Uuid::from_u128(42);
        ctx.owner_paths.borrow_mut().insert(owner, "season".into());
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let rendezvous = (std::sync::Mutex::new(false), std::sync::Condvar::new());
        for limit in [1, 2, 1] {
            setting.store(limit, Ordering::Release);
            peak.store(0, Ordering::Release);
            ctx.excluded.borrow_mut().clear();
            *rendezvous.0.lock().unwrap() = false;
            let rows = map(&[0, 1], &ctx, |parent, ctx| {
                map(&[0, 1, 2], ctx, |child, ctx| {
                    assert_eq!(ctx.owner_paths.borrow().get(&owner).unwrap(), "season");
                    let active = live.fetch_add(1, Ordering::AcqRel) + 1;
                    peak.fetch_max(active, Ordering::AcqRel);
                    if limit == 2 {
                        let mut met = rendezvous.0.lock().unwrap();
                        if active == 2 {
                            *met = true;
                            rendezvous.1.notify_all();
                        }
                        let _wait = rendezvous
                            .1
                            .wait_timeout_while(met, std::time::Duration::from_secs(3), |met| !*met)
                            .unwrap();
                    }
                    let path = format!("{parent}/{child}");
                    ctx.excluded.borrow_mut().push(path.clone());
                    live.fetch_sub(1, Ordering::AcqRel);
                    path
                })
            });
            assert_eq!(rows.concat(), ["0/0", "0/1", "0/2", "1/0", "1/1", "1/2"]);
            assert_eq!(
                peak.load(Ordering::Acquire),
                usize::try_from(limit).unwrap()
            );
            let mut effects = ctx.excluded.borrow().clone();
            effects.sort();
            assert_eq!(effects, rows.concat());
            assert_eq!(budget.extra_workers.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn cancelled_folder_work_stops_admission_and_releases_budget() {
        let naming = super::super::NamingOptions::new();
        let mut ctx = super::super::PlanCtx::new(&naming, super::super::PlanScope::ALL);
        let budget = Budget::new(|| 2);
        ctx.fanout = Some(Arc::clone(&budget));
        let cancel = super::super::ScanCancel::default();
        cancel.cancel();
        ctx.cancel = Some(cancel);
        let result: Vec<Vec<u8>> = map(&[1, 2, 3], &ctx, |_, _| panic!("cancelled work ran"));
        assert_eq!(result, vec![Vec::<u8>::new(); 3]);
        assert_eq!(budget.extra_workers.load(Ordering::Acquire), 0);
    }
}
