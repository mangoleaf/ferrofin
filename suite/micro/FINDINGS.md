# Capacity-cliff investigation — 2026-08-19

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
