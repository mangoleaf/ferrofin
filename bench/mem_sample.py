#!/usr/bin/env python3
"""Sample a container's memory — and the interference on its cores — from the host
cgroup v2 tree and /proc/stat (PLAN_BENCHMARK_V3 §1).

    mem_sample.py CONTAINER OUT_CSV [INTERVAL_MS=100] [CPUSET=8-15]

Columns per sample (interval = one row):
  t        unix seconds
  anon     bytes — the published memory number (heap + stacks of the server and its ffmpeg children)
  file     bytes — page cache, recorded, never published as memory
  current  bytes — memory.current
  cores_busy      fraction of the CPUSET cores' time that was busy (all processes)
  container_cpu   fraction of the CPUSET cores' time used by the container itself
  interference    cores_busy - container_cpu: time on the server's cores not spent by the
                  container — other processes, plus the kernel's own network work for the
                  server's traffic (a few % under load is that, not a neighbour)
  swap     bytes — memory.swap.current (must stay 0: run.sh sets --memory-swap = --memory)
  load1    host 1-min loadavg
Runs until SIGTERM/SIGINT. `--check CPUSET` prints the idle fraction of those cores
over one second and exits (the preflight in run.sh).
"""

import signal
import subprocess
import sys
import time


def cpuset_list(spec):
    out = []
    for part in spec.split(","):
        a, _, b = part.partition("-")
        out.extend(range(int(a), int(b or a) + 1))
    return out


def cpu_times(cores):
    """(busy, total) jiffies summed over the given cores."""
    busy = total = 0
    for line in open("/proc/stat"):
        if line.startswith("cpu") and line[3:4].isdigit() and int(line.split()[0][3:]) in cores:
            v = [int(x) for x in line.split()[1:]]
            idle = v[3] + v[4]  # idle + iowait
            total += sum(v)
            busy += sum(v) - idle
    return busy, total


def cgroup_dir(container):
    cid = subprocess.check_output(["docker", "inspect", "-f", "{{.Id}}", container], text=True).strip()
    for d in (f"/sys/fs/cgroup/system.slice/docker-{cid}.scope", f"/sys/fs/cgroup/docker/{cid}"):
        try:
            open(f"{d}/memory.stat").close()
            return d
        except OSError:
            pass
    sys.exit(f"no cgroup v2 memory.stat for {container}")


def container_cpu_usec(d):
    for line in open(f"{d}/cpu.stat"):
        if line.startswith("usage_usec"):
            return int(line.split()[1])
    return 0


def main():
    if sys.argv[1] == "--check":
        cores = cpuset_list(sys.argv[2])
        b0, t0 = cpu_times(cores)
        time.sleep(1)
        b1, t1 = cpu_times(cores)
        print(f"{1 - (b1 - b0) / max(1, t1 - t0):.3f}")
        return
    container, out = sys.argv[1], sys.argv[2]
    interval = int(sys.argv[3]) / 1000 if len(sys.argv) > 3 else 0.1
    cores = cpuset_list(sys.argv[4]) if len(sys.argv) > 4 else list(range(64))
    hz = 100  # USER_HZ (jiffies per second) on Linux
    d = cgroup_dir(container)
    stop = False

    def on_sig(*_):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGTERM, on_sig)
    signal.signal(signal.SIGINT, on_sig)
    with open(out, "w") as f:
        f.write("t,anon,file,current,cores_busy,container_cpu,interference,swap,load1\n")
        nxt = time.monotonic()
        pb, pt = cpu_times(cores)
        pc = container_cpu_usec(d)
        while not stop:
            try:
                stat = dict(line.split() for line in open(f"{d}/memory.stat"))
                cur = open(f"{d}/memory.current").read().strip()
                try:
                    swap = open(f"{d}/memory.swap.current").read().strip()
                except OSError:
                    swap = "0"
                cc = container_cpu_usec(d)
            except OSError:
                break  # container gone
            b, t = cpu_times(cores)
            dt_j = max(1, t - pt)
            cores_busy = (b - pb) / dt_j
            container_cpu = (cc - pc) / 1e6 * hz / dt_j
            pb, pt, pc = b, t, cc
            load1 = open("/proc/loadavg").read().split()[0]
            f.write(f"{time.time():.3f},{stat['anon']},{stat['file']},{cur},{cores_busy:.3f},{container_cpu:.3f},{max(0.0, cores_busy - container_cpu):.3f},{swap},{load1}\n")
            f.flush()
            nxt += interval
            time.sleep(max(0.0, nxt - time.monotonic()))


if __name__ == "__main__":
    main()
