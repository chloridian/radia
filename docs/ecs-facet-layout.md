# ECS facet layout evaluation

Keep the sparse per-field layout evaluated at `e5275f10b497b00c974dc66d569b3a7614cc0f48`. Neither alternative established a favorable overall tradeoff, so no production layout change was retained.

## Scope and fixture

The fixture is synthetic and does not claim to represent a Second Life population. It builds a 10,000-record World with 32 avatars, 64 attachments, and 100 media-bearing objects. It mixes appearance and shape updates, PBR assignment and override set/clear/re-add, texture animation, stale-source observations, parenting changes, region transfer, local-ID reuse, list and detail reads, and a bounded removal of a 257-object linkset. Criterion repeatedly cycles the same fixture to model a longer session. Results are workload evidence for this fixture only.

Measurements ran on Windows 11 Pro build 26200 with an Intel Core i5-9600KF, 6 cores and 6 logical processors. The optimized Cargo bench profile and identical inputs were used for each layout. Latency runs used `RUSTFLAGS='--cfg world_bench_uninstrumented'`, 30 Criterion samples, a 1-second warmup, and a 5-second measurement target. Allocation and layout diagnostics used separate builds with `RUSTFLAGS='--cfg world_bench_detailed'`; those diagnostics are not latency results.

The matched benchmark commands were:

```powershell
$env:RUSTFLAGS = '--cfg world_bench_uninstrumented'
cargo bench -p world --bench main -- world/facets --sample-size 30 --warm-up-time 1 --measurement-time 5 --noplot

$env:RUSTFLAGS = '--cfg world_bench_detailed'
cargo bench -p world --bench main -- world/facets --sample-size 10 --warm-up-time 1 --measurement-time 3 --noplot
```

## Structural and memory results

| Layout | Archetypes before/after cycle | Facet structural moves per cycle | Retained allocation delta | Peak allocation delta | Allocation/reallocation calls |
| --- | ---: | ---: | ---: | ---: | ---: |
| Sparse `Fenced<T>` components | 11 / 11 | 2 | 34,742,155 B | 41,692,937 B | 192,685 / 41,545 |
| Stable object/avatar bundles | 3 / 3 | 0 | 26,316,791 B | 33,987,340 B | 109,317 / 20,101 |

The bundle candidate reduced the measured retained allocation delta by 24.3% and peak allocation delta by 18.5%. It also reduced allocator calls. This is an allocation-instrumented fixture measurement, not process RSS. The detailed trace returned 10,000 visible objects and 10,000 detail records for each layout. In the detailed one-shot cleanup probe, the sparse layout took 669 µs and the bundle candidate 814 µs; both used 33 turns, 513 work units, and stayed at the 16-unit turn cap. The cleanup probe ran with diagnostic instrumentation and is not used as the latency comparison.

## Allocator-free latency

Each cell gives the Criterion point estimate and its reported interval. The benchmark's measured system busy value is included to show run-to-run load differences.

| Layout and run | System busy | Update cycle | List and detail read | Bounded cleanup |
| --- | ---: | ---: | ---: | ---: |
| Sparse 1 | 34.1% | 4.7645 ms [4.7330, 4.8111] | 14.836 ms [14.744, 14.956] | 517.92 µs [507.71, 530.93] |
| Sparse 2 | 31.0% | 4.9663 ms [4.8638, 5.0759] | 14.967 ms [14.827, 15.172] | 509.12 µs [501.37, 518.27] |
| Bundle 1 | 37.1% | 6.2496 ms [5.7352, 6.8350] | 17.264 ms [15.885, 18.913] | 657.38 µs [560.02, 832.61] |
| Bundle 2 | 41.3% | 5.6780 ms [5.4580, 5.9720] | 15.030 ms [14.589, 15.547] | 523.47 µs [514.20, 533.99] |

An independent Windows CPU-counter sample overlapping Bundle 1 averaged 51.1% total CPU busy and peaked at 79.8%; Bundle 2 averaged 41.0% and peaked at 66.5%. The sparse and bundle runs therefore were not made under closely matched load. The second bundle read and cleanup results are near the sparse measurements, while the update results are slower and load-sensitive. The memory reduction is measurable, but these latency runs do not establish a favorable overall tradeoff. No production rewrite is justified by this evidence.

## Batched-removal candidate

The public trace never supplied a multi-field stored-component removal to batch. Regional facet clears retain their field fences, and `ObjectMedia` retains a value object when it becomes empty. Transfer resets only the `SitTarget` facet. Existing additions are already collected into one `EntityBuilder` insertion. A temporary branch for multi-field removals therefore did not execute in the fixture and was removed.

Two allocator-free repeats of that prototype were also noisy. At 36.7% and 38.8% reported system busy, update cycle estimates were 5.5066 ms and 4.8095 ms; list/detail reads were 16.056 ms and 15.412 ms; bounded cleanup was 516.62 µs and 551.34 µs. It made no archetype or structural-move change on the trace and did not demonstrate a benefit.

## Compatibility and checks

The bundle candidate passed all 244 World tests. These include the independent freshness model (`facet_fences_match_model`), PBR clear/re-add and material freshness, media budget and reads, transfer resets, parenting, local-ID reuse, removal, and bounded cleanup. The viewer inbound object update and transform suites passed 12/12 tests against the candidate; `motion_updates_preserve_physics_and_time_dilation_fields` exercises full, cached full, compressed, cached compressed, and terse paths. The World/render focused baseline runs also passed: 244 World tests and 357 render tests (43 skipped). No live Second Life capture or complete interoperability claim is made.

After removing both layout candidates, final verification is recorded below.

| Command | Result |
| --- | --- |
| `cargo nextest run -p world --all-features --locked` | Passed, 244/244 |
| `cargo nextest run -p render --all-features --locked` | Passed, 357 passed, 43 skipped |
| `cargo clippy -p world --all-targets --all-features --locked -- -D warnings` | Passed |
| `cargo clippy -p render --all-targets --all-features --locked -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `git diff --check` | Passed |
| `RUSTFLAGS='--cfg world_bench_detailed' cargo check -p world --bench main --all-features --locked` | Passed |

The separate 006 geometry candidate remains a final integration prerequisite if a future measured layout winner is proposed. This investigation did not import or change it.
