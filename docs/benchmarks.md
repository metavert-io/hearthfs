# Benchmarks

Measurements of HearthFS's own machinery, each with the method to
reproduce it. How well search finds the right files depends on your
corpus: measure that with the retrieval eval
([retrieval-eval.md](retrieval-eval.md)).

## Indexing throughput

- **Full-text only:** a ~19k-file corpus (837 MB) indexes in a few minutes.
- **With local embeddings:** originally ~77 ms per probe, run serially, or
  about 12 hours for that corpus. Fixed 2026-07-07: the Mutexes around the
  model and tokenizer were unnecessary (tract's `SimplePlan::run` is
  `&self`), and probes now embed on a scoped thread pool, **~4.8× faster**
  (a 200-file corpus went from 460 s to 95 s at 658% CPU; the 19k-file
  corpus takes about 2.5 hours). The next lever, if needed, is batched
  inference.

## Mount transport: FSKit vs NFS read latency (2026-07-11)

The decision-gate benchmark from [fskit-spike.md](fskit-spike.md): adopt
FSKit as the default mount on macOS 26+ if its p95 read latency is under
2× the NFS path. Hardware: an Apple-silicon Mac on macOS 26.5.2. Corpus:
504 text files (17 MB, 1–100 KB each, nested folders) synthesized from repo
text. Five alternating mount cycles per transport, with
`scripts/bench/bench_read.py` timing each file's open+read+close through a
fresh mount (cold = first touch, warm = second pass), after background
indexing had settled. Source files were pre-warmed, so the difference is
transport overhead, not disk.

| metric (mean of 5 runs) | raw local | FSKit | NFS | FSKit/NFS |
|---|---|---|---|---|
| cold read p50 | 0.020 ms | 0.635 ms | 0.342 ms | 1.86× |
| **cold read p95** | 0.032 ms | **1.051 ms** | **0.529 ms** | **1.99×** |
| cold read p99 | 0.055 ms | 1.575 ms | 0.705 ms | 2.23× |
| warm read p95 | 0.027 ms | 0.289 ms | 0.029 ms | 9.85× |
| stat p95 | 0.004 ms | 0.004 ms | 0.004 ms | 1.00× |
| full-tree readdir walk | 1.4 ms | 28.8 ms | 1.2 ms | 24.6× |
| warm throughput | 1618 MB/s | 152 MB/s | 1398 MB/s | 0.11× |

- **Cold reads (the agent workload): 1.99× at p95**, right on the gate
  line. The first mount of each run was slower for both transports;
  counting only steady-state runs (2–5) the ratio is 2.27×. Either way the
  absolute FSKit penalty is **~0.3–0.5 ms per cold read**, which is noise
  next to an LLM turn.
- **Warm reads and readdir favor NFS heavily**, because the kernel NFS
  client caches pages and directory entries while FSKit's path-URL route
  takes an XPC round-trip every time. The absolute numbers stay small
  (warm p95 0.29 ms; 29 ms to walk all 504 files), but re-read-heavy
  workloads notice. A future lever: FSKit volume caching hints, if the API
  grows them.

**Decision: FSKit is the default mount on macOS 26+.** The strict gate was
a wash (1.99× including warm-up, 2.27× steady-state), so qualitative
factors decided. At the time, an early macOS 26 build refused NFS loopback
mounts, and the LAN-IP workaround HearthFS used instead broke off-network,
raced DHCP renewals and exposed a read-only NFS server to the LAN. (The NFS
fallback is loopback-only again since macOS 26.5.2 accepted loopback
mounts; see the CHANGELOG.) Sub-millisecond cold reads through a
supported, local, kext-free path won. NFS remains the transport for
smart sources (Hearthloom, GitHub, API and MCP volumes), for macOS before
26, and for re-read-heavy harnesses such as `scripts/eval/run_stage.sh`,
which benefit from kernel caching.

Reproduce: `python3 scripts/bench/bench_read.py <root> <label>` against a
fresh mount of each transport.
