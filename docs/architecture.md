Radia separates simulator observations, accepted World state, derived scene data, and GPU resources. This document describes responsibilities, ownership, and the contracts between subsystems. Product direction lives in [vision.md](vision.md), compatibility research guidance in [reference.md](reference.md), and development commands in [tooling.md](tooling.md).

## Data flow

Simulator traffic enters through LLUDP circuits and HTTP capabilities. `network` owns delivery and connection lifecycles; `protocol` owns wire types and codecs. The viewer translates messages into domain observations, preserving source identity, ordering evidence, partial updates, and coverage loss. The session-owned `World` validates and applies them. Local commands become authoritative state only when simulator observations confirm them.

Inventory follows a separate domain model under the same session owner. Login supplies roots and folder skeletons. Workers prepare normalized state, folder replacements, and cache restoration from immutable snapshots. The session validates their revisions before publication and applies live deltas in source order. Dependent inventory work waits in a bounded FIFO; independent World traffic continues except at ordered lifecycle barriers.

Render reads World through the viewer's bounded `WorldReader` bridge. Scene reconciliation combines accepted observations with available assets, requests missing content, and publishes sequenced batches to the viewport's `SceneView`. GPU preparation consumes that view without reading network messages or mutating World. Assets publishes immutable CPU content; GPU resources belong to Render.

Transport coverage and scene recovery are distinct. Session intake tracks missing simulator observations and requests recovery where supported. Render can rebuild derived state after history loss, but cannot reconstruct observations World never received. A populated scene does not prove complete simulator state.

## State and execution

The session intake task is the sole owner of mutable World and inventory state. Consumers use bounded requests, immutable snapshots, change journals, and revision notifications rather than locking authoritative state. Cancellation and reserved reads remain serviceable during pending work. Ordered operations retain their continuations before later operations can pass them.

`main` creates the Tokio service runtime and starts GPUI. Networking owns a separate runtime and thread. Asset fetching and scene reconciliation run as service tasks; decoding, geometry generation, inventory preparation, and expensive reclamation use blocking workers. Queues, byte reservations, cancellation, and generation checks bound in-flight work and prevent stale results from crossing lifecycle boundaries.

The scene viewport runs inside GPUI on the shared `wgpu` device. It owns `SceneView` and renderer state, draws to an offscreen texture composited through GPUI's surface element, and handles input, cursor capture, and window lifecycle directly on the GPUI thread. Obsolete session results are canceled or retired before publication.

Shutdown disconnects the session, awaits scene shutdown and retained session-drain tasks, and completes asset and cache cleanup before stopping the service runtime. Drain tasks remain owned across login attempts. GPUI releases its shared cache reference before final cache shutdown.

## Code map

### `apps/viewer`

Composes application startup, GUI, session lifetime, persistence, and subsystem coordinators. It translates protocol messages into World observations and inventory deltas, owns request policy and retries, and returns worker results to the session owner for publication. Its scene feed schedules reconciliation and delivers sequenced batches to the viewport.

The viewport owns input and camera controls. It sends coalesced movement and camera snapshots to the session; focus loss releases held controls immediately. Input never mutates World. The session publishes authoritative self-agent samples, while the viewport predicts the displayed pose and retains client-owned body yaw. Drawing and picking use that same displayed pose. Follow mode tracks the avatar; focusing a picked object enters the independent inspection camera.

The viewer owns the region object-update cache because it owns update-decoding policy. `objects.sqlite` scopes encoded full/compressed updates by grid, region, and cache identity. Identity changes invalidate the old scope, and terse updates invalidate cached payloads that could restore stale motion. Cached updates use the normal decoder and validation path. A matching live UUID does not reapply an older cached observation; missing, corrupt, or identity-mismatched entries request authoritative recovery. Cache reads cannot indefinitely delay the region-handshake reply, and storage failure falls back to an empty cache.

The viewer also persists per-grid avatar names, group names, and verified login username-to-agent mappings in `cache/names.sqlite` under `StorageClass::Cache`, bounded by page count and per-grid entry pruning; cache unavailability degrades gracefully without failing login.

The same scope stores PBR material override snapshots by local ID with the object UUID. Writes follow World's change journal, so live overrides, introductions, rebinds, transfers, kills, and clears share one path; per-scope row and byte limits delete a row instead of exceeding them. For a cached reference without a live object, a stored override is handed to World as a retained override expected for its UUID before the cached batch applies, so a hit introduces the object with its overrides and a miss keeps them for the full update. A retained override from the current session takes precedence. A hit with a different UUID or an undecodable payload deletes the row.

Inventory and asset coordinators own transport scheduling, while domain acceptance remains in their crates. The asset coordinator batches each route's legacy material fetches into `RenderMaterials` requests of at most 50 IDs, one in flight and at least one second apart, waiting longer when the server asks for a retry delay. The chat composer retains drafts rejected by command admission and adds a local transcript entry only after admission succeeds.

### `crates/assets`

Owns decoded textures, materials, environments, mesh headers, and mesh LOD content. Each session's `Store` coordinates demand leases, route eligibility, priorities, retries, progressive refinement, cancellation, and memory reservations. The viewer performs network requests and schedules decoding, keeping asset policy independent of transport.

Store maintenance reconciles coalesced dirty resources and indexed deadlines in bounded turns. A saturated notification queue falls back to a cursor scan. Entry-owned notification identity prevents late lease or work completions from affecting a replacement entry with the same key. Fetch, cache-read, and decode admission have separate limits and share retained-byte accounting. Mesh, legacy material, and other fetches use separate pools. A demanded transient failure restarts after its negative result expires, honouring a longer server retry delay up to five minutes.

Visible content takes priority over background work, with mesh importance reflecting projected size. Request changes update retained demand rather than discarding usable fallbacks. A texture remains usable while higher resolution is pending; ready status reflects current demand. Mesh headers and individual LODs are separate fetch and cache units. A bounded full-response fallback handles servers that do not satisfy range requests. All paths use validated payload and decode reservations.

Unfinished network requests depend on their selected region route and cancel when that route retires. Cache reads and acquired bytes can remain useful while any shared demand survives; a cache miss resumes scheduling through an eligible route. Only successfully decoded network units enter the asynchronous cache writer. Missing or corrupt cached data falls back to network retrieval, while temporary admission pressure defers work.

Assets declares cache namespaces and payload contracts, protects demanded entries with leases, and supplies eviction policy. Storage owns files, indexing, hashing, quotas, locking, and recovery. Generated prim geometry remains in Render's memory cache.

### `crates/cvars`

Owns typed configuration variables (`CvarDescriptor`, `CvarKey<T>`, `CvarRegistry`, `CvarSnapshot`) compiled and validated at build time from `config/cvars/*.toml` and embedded graphics presets from `config/presets/graphics/*.toml` so defaults and presets require no runtime I/O. It resolves values across five precedence layers (`Default`, `Preset`, `Global`, `StartupDeveloper`, `Session`), validates bounds and layer permissions atomically in batches, tracks provenance including pending restart-required changes, and publishes revisioned immutable snapshots. It has no workspace dependencies.

`apps/viewer` persists `Global` overrides to `settings.toml` in the platform local-data directory (`%LOCALAPPDATA%\Radia\settings.toml` on Windows) using `toml_edit` so user comments, formatting, and unknown keys survive saves. Reads are bounded by file size, table count, and key count; malformed files or invalid known values are rejected atomically without modifying the file, falling back to defaults at startup or keeping the last valid configuration on reload. Saves check the on-disk content fingerprint before writing via `utils::write_atomic` on a background worker and abort with a conflict error rather than overwriting external edits. `SessionOnly` variables and `Session` overrides never write to disk, and `LOG` / `FILE_LOG` environment variables retain startup precedence over persisted logging CVars.

### `crates/llsd`

Owns LLSD values, Serde integration, and XML, binary, and notation codecs. Decode limits bound input size, depth, value counts, and scalar/container sizes, including skipped values. Callers can tighten those limits for their admission budgets.

Retained-value construction uses conservative heap accounting, including intermediate duplicate-field values. This accounting does not cover upstream parser buffers or total process memory. The binary codec can decode a prefix and report bytes consumed; whole-document decoding rejects trailing bytes. Domain semantics belong to higher layers.

### `crates/inventory`

Owns personal inventory and Library records, parent/child indexes, link resolution, folder completeness, validated mutation planning, and immutable snapshots. The viewer session is the sole mutation owner. Prepared transactions carry their base revision and folder generation; stale results cannot overwrite intervening live changes.

AIS and legacy folder responses enter the same application path. Legacy decoding separates direct children from auxiliary link targets and preserves supplied parents. Only direct membership replaces the fetched folder; auxiliary targets wait for their actual parent. A reported count must match direct membership before the folder becomes complete. Unknown or conflicting versions leave it incomplete and eligible for reconciliation. The fetch coordinator prioritizes foreground requests and uses bounded background traversal; it does not mutate inventory directly.

Edits run in order and report success only after their confirmed delta is applied by the session owner. Uncertain server outcomes invalidate affected folders and schedule reconciliation. Root, protected-folder, and Library restrictions belong to operation policy rather than storage.

Inventory owns its SQLite schema, SQL, row mapping, and validation. One `inventory.sqlite` serves grid/owner scopes with cache generations. The login skeleton and current versions determine which restored folders may be complete. Live updates and completed fetches win over stale restore batches. Invalid cached state becomes a cache miss or scope invalidation, not authoritative inventory.

The viewer's cache lifecycle separates opening, active writing, disablement, and restore validity. Pending changes pass row/byte admission before retaining snapshots. A serial worker encodes and writes invalidation first, records next, and completeness markers last. Failures advance the affected cache generation. Reserved disposal capacity routes large snapshots and change sets through a bounded reclaimer, including when caching is disabled or shutdown interrupts preparation.

Inventory admission loss is recovered explicitly. Accepted observations drain first; owner-wide refresh invalidates completeness and reconciles every known folder without relying on changed ancestor counts or recursive-fetch depth. Lifecycle and crossing barriers preserve order. Cache completions remain serviceable while restore waits, preventing disposal and write admission from blocking each other.

### `crates/math`

Provides shared geometry and numeric primitives, including selected `glam` types and UV transforms. Frustum operations use zero-to-one clip depth. Ray intersections preserve the parameter in `origin + direction * t`, including for unnormalized directions. Subsystem policy stays with its owning crate.

### `crates/network`

Owns login, sessions, simulator connections, capability traffic, and transport coverage. Each circuit owns its simulator socket, LLUDP delivery and ordering, send pacing, and bounded packet admission. Session events carry source-specific ordering evidence and retained-memory charges.

A session uses one `SimulatorId`-keyed record map for routes, task handles, capabilities, EventQueue state, budgets, and pre-connect events. The primary simulator is an ID into that map. Promotion, retirement, and task completion act on the same records; generation checks prevent stale completion from changing a replacement connection. Startup handshake evidence remains independent of connection lifecycle state.

Reliable application packets acquire admission credit before commit and acknowledgment. Pressure leaves them available for simulator retransmission and records coverage loss. Control traffic and reserved lifecycle capacity remain separate; sustained pressure or exhausted lifecycle admission ends the session with a typed reason. Inventory admission loss is coordinated with the viewer's ordered recovery, rather than silently replaying rejected observations or buffering without limit.

Ordinary capabilities and asset requests have separate HTTP budgets. Range requests retain bounded partial/full-response handling and their selected redirect policy. EventQueue reserves primary-simulator capacity and separately bounds long polls, response bodies, decoding, and retained events. Body permits remain with blocking decode even after the async waiter cancels, then release before event publication can wait. Simulators send material overrides only to viewers that request `ModifyMaterialParams`, so each simulator's first seed request asks for it alongside `EventQueueGet`. Parser limits also apply to ignored fields; raw response storage and retained-event memory are accounted separately.

Region handshakes reach the viewer, which resolves cache identity and sends a reliable reply. Handoff tracks its destination and role generation. Logout distinguishes local send completion from a matching remote reply and keeps receiving until the reply or its bounded deadline. World, inventory semantics, and rendering belong outside Network.

### `crates/protocol`

Owns Second Life wire types, LLUDP framing, field codecs, and typed messages grouped by protocol area. Each message owns its codec and allocation preflight; a declarative list supplies variants and dispatch. Verified vendored template metadata remains the fallback for untyped messages. External spellings and layouts stay exact.

Protocol validates framing and wire values; World and Inventory decide domain acceptance. Inventory-specific bounded Serde visitors build typed records without retaining a generic tree, correlate identities with the request, and separate legacy children by supplied parent. Request scheduling and batch size belong to the viewer.

### `crates/render`

Derives scene state from World and Assets and owns GPU rendering. Reconciliation reads bounded World pages and journals, selects detail, generates prim/sculpt/terrain/water geometry, resolves materials, and requests assets. It publishes sequence-checked batches and ready handles through `SceneView`. In-flight reads carry an epoch and request identity; late results cannot clear replacement reads or publish after recovery. Applied receipts advance publication, while abandoned deliveries trigger resynchronization.

World reads distinguish an unfinished page from completion, including empty filtered pages. Render must resume their cursors. Recovery preserves the previous drawable while rebuilding derived state, but immediately excludes removed objects, invalidated subtrees, and retired regions.

Mesh demand shares asset headers and LOD states across objects and region routes. Desired detail stays distinct from the published fallback. A pending LOD retains usable geometry until replacement is ready; absent or unavailable LODs follow the supported fallback order. Material slots follow the published mesh. Camera scans, dirty objects, prim generation, and asset completions share bounded preparation so one workload cannot monopolize progress. Texture demand can start before geometry completes, and visible uploads precede background uploads. GPU mesh uploads follow one demand order over the range: meshes of items in view first, then the rest by distance and size. Only demand in view may evict.

Geometry preparation caches transforms, bounds, materials, and faces. It separates replacement bounds from the currently displayed bounds and keeps the displayed mesh when refinement cannot fit. The committed draw list holds every eligible in-range item with a resident mesh. Prim mesh preparation and mesh asset requests cover the same range; requests for meshes beyond the draw distance run at background priority. Camera rotation changes only the per-frame cull and transparency ordering; camera position changes range membership and detail demand. Drawing and picking use the same cached placement, including predicted self-avatar motion. Object range limits remain separate from terrain and water frustum eligibility.

Eviction skips meshes the camera currently shows and meshes just admitted for items in view. It takes meshes no range item wants first, then the lowest-ranked range demand. Evicting a mesh disables every committed and staged command that uses it before the mesh is released, and the release waits for submitted frames to finish. Admission and retirement remain bounded; capacity exhaustion can publish the ready subset with explicit omissions rather than waiting forever. Canceled layouts retire incrementally. Each view culls the committed commands every frame, by spatial cell and command bounds, into its own indirect draws; the commands change only when a layout commits. Geometry recovery uses bounded pages and continuations after history loss, and subsequent changes coalesce for follow-up work. These work limits are not hard frame-time guarantees.

Committed drawing owns its coordinate frame. Each face's instance is one storage-buffer record holding its transform and material parameters, indexed by its layout slot. The instance buffer starts small and doubles when a committing layout needs more slots. Growth stops at the smaller of the device's storage-binding limit and a 64 MiB instance budget that covers the instance, face-index, and indirect buffers; the last step stops at that cap instead of doubling past it. A grown buffer receives the whole committed layout before its first draw. Layouts beyond the cap omit faces by draw priority. The instance budget is separate from the mesh memory budget. Instance transforms and camera/environment uniforms switch origin together when a replacement layout commits. Each face addresses its instance per layout generation, so pose and parameter patches keep updating the committed instance in the active origin while a replacement is staged, and also update the staged instance in the candidate origin. Hiding a face clears its committed instance and invalidates a replacement that already staged it. Culling bounds are world-space, so an origin switch keeps them. Each view sorts the transparent records it keeps back to front. A record remains valid only while its current item face still matches the recorded group, order, and blended state; stale records are skipped, so reused identities never enter draw commands.

Geometry appears as soon as its mesh is resident, using fallbacks until its materials and textures arrive. Later refinement preserves usable content. Dynamic texture animation and predicted motion schedule useful redraws; static scenes return to change-driven rendering. Motion starts from accepted, receive-time-stamped samples and does not change World state. Prediction respects circuit liveness, loaded-region boundaries, terrain, and parent relationships.

Environment evaluation combines World region/parcel assignments, ordered experience-property events, and the session-local choice. Receive times survive delayed reconciliation so transitions retain their intended timing. The viewer supplies agent region and altitude; Render evaluates the environment and owns the sky, atmosphere, clouds, celestial bodies, water, and surface-lighting passes. Region and void water share shading, while void coverage excludes submitted region-water rectangles.

Terrain composition uses region settings, height, globally positioned noise, and the reference ramp. Sampling and normals account for loaded neighboring patches and regions. Terrain and legacy surfaces share atmosphere-aware shading; missing detail textures use fallbacks. All surfaces use the same infinite reversed-Z projection. Scene color stays linear through composition, then exposure, tone mapping, and sRGB encoding apply at the output boundary. Opaque fullbright surfaces take the exposure class of the surface behind them, as in the official viewer. Transparent surfaces preserve the depth owner's scene classification, and post-processing does not apply water fog twice.

PBR emissive radiance joins lit color before atmospheric haze, for opaque and blended surfaces alike. Until a referenced emissive image arrives, a mid-grey placeholder stands in for it. PBR materials that declare `KHR_materials_unlit` render lit, as in the official viewer. Texture-entry glow is not rendered, and distant terrain appearance lacks the legacy viewer's cached low-resolution composite. These two are compatibility limits, not completed rendering features. Water differs from the official viewer in three ways: wave input is not capped at 2560 m, the fog sample extends 2048 m from the water intersection rather than from the camera, and reflections use a sky-only fallback without reflection probes. Texture mip chains average encoded channels for compatibility, so color and raw-channel views share one chain.

### `crates/storage`

Owns persistent payload bytes, metadata indexing, integrity checks, quotas, process locking, recovery, and eviction mechanics. Consumers declare validated namespaces, keys, and payload bounds, then validate the domain meaning of returned bytes. Asset payloads live in their declared directories with metadata in `assets/index.sqlite`; Inventory and the viewer's object cache own separate databases.

SQLite connections stay on dedicated workers. Databases open under an explicit storage class: `Persistent` (`synchronous=FULL`, no destructive recovery), `Journal` (`synchronous=NORMAL`, no destructive recovery), `Cache` (`synchronous=NORMAL`, recreated on corruption or older-schema mismatch), or `Ephemeral` (`synchronous=OFF`, recreated on corruption or older-schema mismatch). Schema checks and migrations run inside an immediate write transaction, and newer unknown schema versions fail without deleting data. Command queues and transactions have row/byte bounds; reserved control capacity permits recovery without unbounded retention. Reads use the same worker. Busy writes return typed outcomes, and aggregate counters track contention, queue rejections, failed writes, and recoveries. Repair requires exclusive maintenance ownership before removing database/WAL files, and worker failure closes admission and resolves pending receipts.

Payload lookups use the bounded in-memory index and validate requested size, file length, and hash. The writer stages replacement files beside their destination, atomically renames them, then commits metadata. Interrupted writes leave either verifiable rows or orphan payloads for a bounded sweep. Lookup corruption invalidates the entry rather than returning unchecked bytes.

Storage controls victim selection and quota enforcement; consumers can supply ranking policy. Demand-protection leases prevent eviction until their final consumer releases them. Eviction, orphan cleanup, and changed-limit enforcement run in bounded batches and resume from retained cursors. Failed maintenance backs off, and unchanged protected candidates do not cause repeated idle scans. Shutdown joins workers and reports failures.

### `crates/test-support`

Provides shared protocol vectors, texture and mesh fixtures, allocation measurements, and subprocess helpers with bounded readiness and cleanup. Production crates depend on it only for development and verification. Tests must distinguish generated inputs from independently sourced compatibility evidence.

### `crates/utils`

Provides shared ID newtypes and general helpers such as atomic file replacement and rate-limited summaries for hot-path logging. Domain validity stays with the owning type, and Utils has no subsystem dependencies. Replacement retries preserve the existing destination on failure and share a bounded retry budget across a cache commit.

### `crates/world`

Owns accepted simulator state in ECS: regions, objects, relationships, poses, appearances, terrain, media, environment, and parcels. Mutations validate and plan before committing state and indexes. Full observations and partial patches share field-level authority and freshness rules. Optional `Fenced<T>` components retain their ordering fences when values clear; transfers reset only fields whose contract requires it.

World issues opaque region handles scoped to its lifetime; the session maps simulator identities to them. Entity generations, source clocks, tombstones, and address/parent fences distinguish stale updates, deletion, local-ID reuse, and handoff. Omitted fields remain distinct from explicit clears or zeros. Material overrides that arrive before their object are retained per region within fixed bounds, applied when that object is introduced, and released by a newer kill or by reclamation of their retired region. Parcel overlay parts stay private until a complete replacement can publish, and stale replies cannot overwrite the current parcel.

Parent resolution is ordered and resumable. Missing parents leave children pending, cycles cannot commit, and children whose parent binding changes retain identity evidence for later recovery. Each pass over waiting children, including the immediate and preflight passes inside a mutation, shares one bounded work allowance across parent and child visibility checks and ancestry steps. A queued job keeps its continuation only while the revision and parent identity are unchanged. The session rotates relationship jobs between intake turns rather than resolving whole subtrees synchronously.

Kill, address replacement, transfer, and region retirement install logical fences before physical cleanup. Reclamation incrementally releases descendants, relationships, components, and region data. Retained records remain charged until released; entity and address generations keep old cleanup from affecting replacements. Eligible child avatars detach while preserving a resolvable world pose; otherwise their old local pose is invalidated.

Scene list, detail, and point reads use bounded visibility work. Point lookup returns visible, hidden, or a resumable cursor. An ordered mutation waits for its lookup before the next operation passes it, while the owner can still serve cancellation and reserved reads. Revision changes restart affected cursors, and a bounded revision-keyed memo shares ancestry results. The production read barrier uses a conservative count bound; exact whole-world counts remain explicit queries.

World owns these continuations and accepted state, not persistence, transport recovery, asset fetching, prediction, or GPU policy. Consumers resynchronize when their bounded change history is no longer available.

## Keeping this file sharp

Describe current responsibilities, ownership, data flow, and cross-subsystem invariants. Give each fact one home. Keep local algorithms, numeric constants, fixture details, benchmark results, execution history, and future implementation proposals out of this document. Use source for implementation details and the linked project documents for their own subjects.
