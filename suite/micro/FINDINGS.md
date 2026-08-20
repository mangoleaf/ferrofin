# Capacity-cliff investigation — 2026-08-19

> **RESOLVED.** Root cause was SQLite's global allocator mutex. See "The answer"
> at the bottom; the analysis above is kept because the eliminations are what
> made the cause findable.

Measured with the fast loop (`./serve.sh` + `./hit.sh`), profiling binary over a
copy of the benchv2 database (9,862 items / 7,505 people), native, not Docker.

**Host caveat, read this first.** The box was running clickhouse (~335% CPU),
llama-server, a browser and k3s throughout, and separately had 24 stray
busy-loop shells pinning 24 cores for ~2h46m earlier in the day (killed before
these runs). Absolute numbers are therefore soft. The *shapes* below reproduced
across repeated runs and are what the conclusions rest on.

## What the collapse is not

Ruled out by measurement, each with the observation that killed it:

| hypothesis | verdict |
|---|---|
| DB pool starvation | Pool is 32; a **smaller** pool is faster (see below) |
| CPU capacity | Collapses at 1.6 cores of 32 |
| Allocator (glibc arena) contention | jemalloc is already the global allocator |
| jemalloc `madvise` churn | `dirty_decay_ms:-1,muzzy_decay_ms:-1` changed nothing |
| Auth / session path | `user_me` sustains **3000/s at 0.1 ms** |
| Image or user-data prefetch | `enableImages=false` / `enableUserData=false` change nothing |
| Response body size | 49 KB for 100 items — modest |
| Rollback-journal serialization | DB is in WAL, `locking_mode=normal` |
| The `/Persons` query plan | Indexed seek, no scan (`EXPLAIN QUERY PLAN`) |

## What it is

Capacity is **flat then cliff**, at an effective parallelism of ~1–2 on a
32-core box:

```
items_mixed (100 items/page), pool=32
  100/s   5.5 ms   0.47 user + 0.20 kernel cores
  150/s   5.9 ms   0.79 + 0.31
  200/s   6.3 ms   1.09 + 0.48
  250/s   1547 ms   <- cliff
  400/s   11873 ms  4.2 user + 29.3 kernel cores (87% kernel)
```

Per-request user CPU is 5.45 ms and capacity is ~220/s — i.e. exactly
`1 / service_time`. Something serializes the DB/DTO path. Above the cliff the
process burns the whole machine at **87–90% kernel time** with **5,653 voluntary
context switches per request** (2.5M/s); that is the queueing symptom, not the
cause.

Two endpoints confirm it is one shared resource, not per-endpoint: `items_mixed`
and `persons` are each healthy alone at 150/s, but run **concurrently** at 150/s
each (300/s combined, over the shared ~220/s budget) both collapse to ~1.9 s p95.

Scaling is by item count, not request count — `limit=10` is healthy at 300/s
(7.2 ms) where `limit=100` is 3,843 ms.

## The one actionable lever found: pool size is backwards

`items_mixed` p50, by `FERROFIN_DB_POOL`:

| pool | 300/s | 600/s | 1000/s |
|---|---|---|---|
| 2 | 99 ms | 6103 | 10527 |
| **4** | **7.7 ms** | 5008 | 10841 |
| 8 | 612 ms | 8817 | 16084 |
| 16 | 2102 ms | 11714 | 20226 |
| 32 | 3383 ms | 14227 | 16793 |

Knee: **pool=4 ≈ 350/s** vs **pool=32 ≈ 220/s** — about **1.6× capacity from one
default**, reproduced twice. 32 SQLite reader threads (each a dedicated OS
thread running synchronous C) doing CPU-heavy work simultaneously thrash each
other; four of them queue cleanly instead.

**This contradicts the recorded finding that `auto = cores` is optimal**, so it is
deliberately NOT applied as a default change here. The prior result was measured
on an idle host with `suite/perf/pool_sweep.py`; this one was not. Re-derive with
that tool on a quiet machine before changing `default_pool_size()`
(`crates/ferrofin-db/src/database.rs:534`). The knob already exists
(`FERROFIN_DB_POOL` / `db_pool`), so operators on busy hosts can set it today.

## Blocked, and what unblocks it

Effective parallelism of ~2 on 32 cores is still unexplained. Finding it needs a
profiler, and both routes are closed on this host:

```sh
echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid   # unblocks samply
echo 0 | sudo tee /proc/sys/kernel/yama/ptrace_scope     # unblocks eu-stack/gdb
```

With either one, `./serve.sh profile` then `./hit.sh items_mixed 400` gives the
answer directly. Without them, `samply` refuses and `eu-stack` returns
"Operation not permitted" for every thread.

## Re-measurement note

Everything the full suite reported earlier in the day is suspect — the stray
busy-loops overlapped much of it, including runs reported as clean passes. Only
the *shapes* above, re-measured after killing them, should be trusted.


---

# The answer

Profiling (`eu-stack` sampling, once `ptrace_scope` was lowered) found **132 of 828
stacks blocked on a mutex**, and every caller was SQLite internals:
`sqlite3Malloc` 49, `sqlite3_free` 29, `pcache1Fetch` 25, `pcache1Unpin` 23.

Two global locks, fixed in two steps:

1. **Shared page cache.** `PRAGMA mmap_size = 1 GiB` serves pages from an mmap
   instead of `pcache1`. Re-profiling showed pcache contention drop from 54
   samples to 3, and 400/s went 8,954 ms → 4,712 ms.

2. **Global allocator mutex.** SQLite's default build wraps every
   malloc/free in a global mutex solely to keep `sqlite3_memory_used()`
   accurate — statistics nothing here reads. `sqlite3_config(SQLITE_CONFIG_MEMSTATUS, 0)`
   removes it. After step 1 this was 82 of 85 remaining blocked stacks.

## Result

`items_mixed` (100-item page), p50:

| rate | before | after |
|---|---|---|
| 200/s | 5.0 ms | 2.9 ms |
| 400/s | **8,954 ms** | **2.8 ms** |
| 1000/s | — | 2.8 ms |
| 3000/s | — | 3.5 ms, 100% ok |

The cliff is gone: flat to 3000/s where capacity was ~220/s. The endpoints this
investigation started from, at their calibrated rates, all 100% ok:
`persons` @608/s 2.6 ms (was 8,521 ms), `nextup` @1849/s 2.3 ms (was 10,190 ms),
`items_resume` @464/s 0.2 ms, `music_genres` @728/s 0.8 ms.

## The pool finding above was an artifact — disregard it

This document previously reported `pool=4` beating `pool=32` by 1.6x and
suggested re-deriving the default. That was the contention showing through:
fewer SQLite threads meant less pressure on the one global lock. With the lock
gone, measured again:

| pool | 400/s | 1500/s |
|---|---|---|
| 4 | 2.6 ms | 521.9 ms |
| 32 | 2.7 ms | 2.8 ms |

`pool = cores` is optimal, exactly as the earlier pool-sweep concluded. The
default was right and was correctly left alone.

## Harness gap

`playlist_items` and `shows_seasons` cannot be driven by `hit.sh` yet —
`benchlib.pick_items` does not populate `playlistId`/`seriesId`. Server-side
they go through the same path as the endpoints above, but they are unverified.
