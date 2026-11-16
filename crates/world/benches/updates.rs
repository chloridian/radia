#![allow(unexpected_cfgs)]

use std::hint::black_box;
use std::num::NonZeroU8;
#[cfg(world_bench_detailed)]
use std::time::Instant;

use criterion::{BatchSize, BenchmarkGroup, BenchmarkId, Criterion, Throughput, measurement::WallTime};
use math::{Quat, Vec3};
use utils::AssetId;
use uuid::Uuid;
use world::{
    Appearance,
    ApplyOutcome::{IgnoredStale, Unchanged},
    AttachmentPoint, AvatarAppearanceUpdate, AvatarTextureSlots, AvatarVisualParamIndex, FaceAppearance, FieldUpdate, LinearMotion,
    MAX_OBJECT_DETAIL_BATCH_ITEMS, MAX_OBJECT_PAGE_ITEMS, MAX_REMOVAL_WORK_PER_TURN, MediaEntry, ObjectMediaUpdate, ObjectPatch,
    ParentUpdate::{Local, Root},
    PbrMaterialAssignments, PbrMaterialOverride, PbrMaterialOverrides, Pose, Shape, SitTarget, SurfaceAppearance, TextureAnimation,
};

use super::support::{
    INITIAL_ORDER, PER_REGION, Row, SCENE_SIZES, Scene, observation, observation_with_pose, populated_scene, populated_scene_with_avatars,
    size_name, source,
};

pub(super) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("world/updates");
    bench_transform(&mut group);
    bench_observation(&mut group);
    bench_content_updates(&mut group);
    bench_avatar_updates(&mut group);
    bench_duplicate(&mut group);
    bench_stale(&mut group);
    bench_mixed(&mut group);
    group.finish();

    bench_facet_layout(c);
}

const LAYOUT_OBJECTS: usize = 10_000;
const LAYOUT_AVATARS: usize = 32;

struct FacetWorkload {
    scene: Scene,
    pbr_rows: Vec<usize>,
    media_rows: Vec<usize>,
    attachment_rows: Vec<usize>,
    order: u64,
    round: u32,
    next_id: u128,
    surface: SurfaceAppearance,
    materials: PbrMaterialAssignments,
    overrides: PbrMaterialOverrides,
    media: MediaEntry,
    visual_param: AvatarVisualParamIndex,
}

impl FacetWorkload {
    fn new() -> Self {
        let mut scene = populated_scene_with_avatars(LAYOUT_OBJECTS, LAYOUT_AVATARS, true);
        let surface = SurfaceAppearance::new(
            FaceAppearance { texture: Some(AssetId::new(Uuid::from_u128(401))), ..FaceAppearance::default() },
            vec![(1, FaceAppearance { texture: Some(AssetId::new(Uuid::from_u128(402))), ..FaceAppearance::default() })],
        )
        .expect("valid layout surface");
        let materials = PbrMaterialAssignments::new(vec![(1, AssetId::new(Uuid::from_u128(403)))]).expect("valid layout PBR assignments");
        let overrides =
            PbrMaterialOverrides::new(vec![(1, PbrMaterialOverride { roughness: Some(0.5), ..PbrMaterialOverride::default() })])
                .expect("valid layout PBR overrides");
        let media = MediaEntry {
            current_url: "https://media.invalid/synthetic".to_owned(),
            home_url: "https://media.invalid/home".to_owned(),
            ..MediaEntry::default()
        };
        let media_update = ObjectMediaUpdate::new(1, vec![Some(media.clone())]).expect("valid layout media");
        let animation = TextureAnimation::from_wire(&[0; 16]).expect("valid layout animation");
        let visual_param = AvatarVisualParamIndex::new(0).expect("valid avatar parameter index");
        let mut order = INITIAL_ORDER + PER_REGION as u64 + 1;
        let pbr_rows = (LAYOUT_AVATARS..LAYOUT_OBJECTS).filter(|index| index % 8 == 0).collect::<Vec<_>>();
        let media_rows = (LAYOUT_AVATARS..LAYOUT_OBJECTS).filter(|index| index % 100 == 0).collect::<Vec<_>>();
        let attachment_rows = (LAYOUT_AVATARS..LAYOUT_AVATARS + 64).collect::<Vec<_>>();

        for (index, row) in scene.rows.iter().copied().enumerate() {
            let mut patch = ObjectPatch::new(source(row.region, order), row.local_id);
            order += 1;
            if row.kind == world::Kind::Avatar {
                patch.avatar_appearance =
                    Some(AvatarAppearanceUpdate::new(None, None, vec![(visual_param, 0)], None).expect("valid avatar update"));
            } else {
                patch.shape = FieldUpdate::Set(Shape::Mesh { asset_id: AssetId::new(Uuid::from_u128(400)), flags: Default::default() });
                patch.appearance = FieldUpdate::Set(Appearance::Surface);
                patch.surface_appearance = Some(surface.clone());
                if index % 8 == 0 {
                    patch.pbr_materials = Some(materials.clone());
                    patch.pbr_material_overrides = Some(overrides.clone());
                }
                if index % 100 == 0 {
                    patch.object_media = Some(media_update.clone());
                }
                if index % 50 == 0 {
                    patch.texture_animation = FieldUpdate::Set(animation);
                }
            }
            let _ = scene.world.patch_object(patch).expect("seed layout facets");
        }

        for (offset, index) in attachment_rows.iter().copied().enumerate() {
            let row = scene.rows[index];
            let avatar = scene.rows[offset % LAYOUT_AVATARS];
            let mut patch = ObjectPatch::new(source(row.region, order), row.local_id);
            order += 1;
            patch.parent = Some(Local(avatar.local_id));
            patch.attachment_point = FieldUpdate::Set(AttachmentPoint::new(NonZeroU8::new(1).expect("nonzero attachment point")));
            let _ = scene.world.patch_object(patch).expect("seed layout attachment");
        }

        for index in 256..320 {
            let row = scene.rows[index];
            let target = SitTarget { position: Vec3::ZERO, rotation: Quat::IDENTITY };
            let _ = scene.world.patch_sit_target(source(row.region, order), row.id, Some(target)).expect("seed layout sit target");
            order += 1;
        }
        let transfer_row = scene.rows[LAYOUT_OBJECTS - 2];
        let _ = scene
            .world
            .patch_sit_target(
                source(transfer_row.region, order),
                transfer_row.id,
                Some(SitTarget { position: Vec3::ZERO, rotation: Quat::IDENTITY }),
            )
            .expect("seed transfer sit target");
        order += 1;

        assert_eq!(scene.world.object_count(), LAYOUT_OBJECTS);
        Self {
            scene,
            pbr_rows,
            media_rows,
            attachment_rows,
            order,
            round: 0,
            next_id: LAYOUT_OBJECTS as u128 + 1,
            surface,
            materials,
            overrides,
            media,
            visual_param,
        }
    }

    fn cycle(&mut self) -> usize {
        let mut operations = 0;
        for index in self.pbr_rows.iter().copied() {
            let row = self.scene.rows[index];
            let mut patch = ObjectPatch::new(source(row.region, self.order), row.local_id);
            self.order += 1;
            patch.pbr_materials = Some(PbrMaterialAssignments::default());
            patch.pbr_material_overrides = Some(PbrMaterialOverrides::default());
            let _ = black_box(self.scene.world.patch_object(patch).expect("clear layout PBR values"));
            operations += 1;
        }
        if let Some(index) = self.pbr_rows.first().copied() {
            let row = self.scene.rows[index];
            let mut stale = ObjectPatch::new(source(row.region, row.order - 1), row.local_id);
            stale.pbr_materials = Some(self.materials.clone());
            stale.pbr_material_overrides = Some(self.overrides.clone());
            assert_eq!(self.scene.world.patch_object(stale).expect("stale layout PBR re-add").outcome, Unchanged);
            assert!(
                self.scene.world.object(self.scene.world.entity(row.id).expect("cleared layout object")).unwrap().pbr_materials().is_none()
            );
            operations += 1;
        }
        for index in self.pbr_rows.iter().copied() {
            let row = self.scene.rows[index];
            let mut patch = ObjectPatch::new(source(row.region, self.order), row.local_id);
            self.order += 1;
            patch.pbr_materials = Some(self.materials.clone());
            patch.pbr_material_overrides = Some(self.overrides.clone());
            let _ = black_box(self.scene.world.patch_object(patch).expect("re-add layout PBR values"));
            operations += 1;
        }

        let media_update =
            ObjectMediaUpdate::new(self.round.saturating_add(2), vec![Some(self.media.clone())]).expect("valid layout media update");
        for index in self.media_rows.iter().copied() {
            let row = self.scene.rows[index];
            let mut patch = ObjectPatch::new(source(row.region, self.order), row.local_id);
            self.order += 1;
            patch.object_media = Some(media_update.clone());
            let _ = black_box(self.scene.world.patch_object(patch).expect("update layout media"));
            operations += 1;
        }

        let weight = (self.round % 255) as u8;
        for index in 0..LAYOUT_AVATARS {
            let row = self.scene.rows[index];
            let mut patch = ObjectPatch::new(source(row.region, self.order), row.local_id);
            self.order += 1;
            patch.avatar_appearance =
                Some(AvatarAppearanceUpdate::new(None, None, vec![(self.visual_param, weight)], None).expect("valid layout avatar update"));
            let _ = black_box(self.scene.world.patch_object(patch).expect("update layout avatar"));
            operations += 1;
        }

        let parent_offset = usize::from(self.round % 2 == 1);
        for (offset, index) in self.attachment_rows.iter().copied().enumerate() {
            let row = self.scene.rows[index];
            let avatar = self.scene.rows[(offset + parent_offset) % LAYOUT_AVATARS];
            let mut patch = ObjectPatch::new(source(row.region, self.order), row.local_id);
            self.order += 1;
            patch.parent = Some(Local(avatar.local_id));
            let _ = black_box(self.scene.world.patch_object(patch).expect("reparent layout attachment"));
            operations += 1;
        }

        for row in self.scene.rows.iter().step_by(17).copied() {
            let mut patch = ObjectPatch::new(source(row.region, row.order - 1), row.local_id);
            patch.pose = Some(Pose { position: Some(Vec3::splat(-1.0)), rotation: None });
            assert_eq!(self.scene.world.patch_object(patch).expect("stale layout update").outcome, Unchanged);
            operations += 1;
        }

        let transfer_index = LAYOUT_OBJECTS - 2;
        let row = self.scene.rows[transfer_index];
        let target = SitTarget { position: Vec3::ONE, rotation: Quat::IDENTITY };
        let _ = self.scene.world.patch_sit_target(source(row.region, self.order), row.id, Some(target)).expect("refresh layout sit target");
        self.order += 1;
        let destination = if row.region == self.scene.regions[0] { self.scene.regions[1] } else { self.scene.regions[0] };
        let moved = Row { region: destination, order: self.order, ..row };
        let _ = black_box(
            self.scene
                .world
                .observe_object(observation_with_pose(moved, Pose { position: Some(Vec3::ONE), rotation: Some(Quat::IDENTITY) }))
                .expect("transfer layout object"),
        );
        self.order += 1;
        self.scene.rows[transfer_index] = moved;
        operations += 2;

        let reuse_index = LAYOUT_OBJECTS - 1;
        let row = self.scene.rows[reuse_index];
        let removal_order = self.order;
        self.order += 1;
        let _ = black_box(
            self.scene.world.remove_object(source(row.region, removal_order), row.local_id).expect("remove layout object for reuse"),
        );
        while self.scene.world.has_pending_removals() {
            let progress = self.scene.world.reclaim_removals(MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units <= MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units > 0 || !progress.pending);
            drop(progress);
        }
        let replacement = Row { id: Uuid::from_u128(self.next_id), order: self.order, ..row };
        self.next_id += 1;
        self.order += 1;
        let _ = black_box(
            self.scene
                .world
                .observe_object(observation_with_pose(replacement, Pose { position: Some(Vec3::ZERO), rotation: Some(Quat::IDENTITY) }))
                .expect("reuse layout local ID"),
        );
        let mut patch = ObjectPatch::new(source(replacement.region, self.order), replacement.local_id);
        self.order += 1;
        patch.shape = FieldUpdate::Set(Shape::Mesh { asset_id: AssetId::new(Uuid::from_u128(400)), flags: Default::default() });
        patch.appearance = FieldUpdate::Set(Appearance::Surface);
        patch.surface_appearance = Some(self.surface.clone());
        let _ = black_box(self.scene.world.patch_object(patch).expect("restore reused layout content"));
        self.scene.rows[reuse_index] = replacement;
        operations += 3;

        self.round = self.round.saturating_add(1);
        assert_eq!(self.scene.world.object_count(), LAYOUT_OBJECTS);
        operations
    }

    fn read(&self) -> (usize, usize) {
        let _ = self.scene.world.read_barrier();
        let mut cursor = None;
        let mut entities = Vec::with_capacity(LAYOUT_OBJECTS);
        loop {
            let page = self.scene.world.object_list_page(cursor, MAX_OBJECT_PAGE_ITEMS).expect("read layout objects");
            entities.extend(page.objects.iter().map(|object| object.entity));
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(entities.len(), self.scene.world.object_count());
        let mut detail_count = 0;
        for batch in entities.chunks(MAX_OBJECT_DETAIL_BATCH_ITEMS) {
            let mut detail_cursor = None;
            loop {
                let page = self.scene.world.object_detail_batch_page(batch, detail_cursor).expect("read layout object details");
                detail_count += page.batch.details.len();
                detail_cursor = page.next;
                if detail_cursor.is_none() {
                    break;
                }
            }
        }
        assert_eq!(detail_count, entities.len());
        (entities.len(), detail_count)
    }

    fn operations_per_cycle(&self) -> usize {
        self.pbr_rows.len() * 2
            + 1
            + self.media_rows.len()
            + LAYOUT_AVATARS
            + self.attachment_rows.len()
            + self.scene.rows.len().div_ceil(17)
            + 5
    }
}

fn bench_facet_layout(c: &mut Criterion) {
    let mut group = c.benchmark_group("world/facets");
    #[cfg(world_bench_detailed)]
    {
        let mut profile = FacetWorkload::new();
        let (archetypes_before, _) = profile.scene.world.take_bench_layout_stats();
        let update_ops = profile.cycle();
        let (archetypes_after, facet_moves) = profile.scene.world.take_bench_layout_stats();
        eprintln!(
            "facet-layout-trace fixture=synthetic_10k_sparse before_archetypes={archetypes_before} after_archetypes={archetypes_after} facet_structural_moves={facet_moves} update_ops_per_cycle={update_ops}",
        );
        let (visible, details) = profile.read();
        eprintln!("facet-layout-read fixture=synthetic_10k_sparse visible={visible} details={details}");

        for index in 1..=256 {
            let child = profile.scene.rows[100 + index];
            let mut patch = ObjectPatch::new(source(child.region, profile.order), child.local_id);
            profile.order += 1;
            patch.parent = Some(Local(profile.scene.rows[100].local_id));
            let _ = profile.scene.world.patch_object(patch).expect("link cleanup profile");
        }
        let root = profile.scene.rows[100];
        let started = Instant::now();
        let _ = profile.scene.world.remove_object(source(root.region, profile.order), root.local_id).expect("remove cleanup profile root");
        let mut turns = 0;
        let mut total_work = 0;
        let mut maximum_work = 0;
        while profile.scene.world.has_pending_removals() {
            let progress = profile.scene.world.reclaim_removals(MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units <= MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units > 0 || !progress.pending);
            turns += 1;
            total_work += progress.work_units;
            maximum_work = maximum_work.max(progress.work_units);
            drop(progress);
        }
        assert_eq!(profile.scene.world.object_count(), LAYOUT_OBJECTS - 257);
        eprintln!(
            "facet-layout-cleanup fixture=synthetic_10k_257_linkset elapsed_us={} turns={turns} work_units={total_work} max_work_units_per_turn={maximum_work} cap={MAX_REMOVAL_WORK_PER_TURN}",
            started.elapsed().as_micros(),
        );
    }
    #[cfg(not(world_bench_uninstrumented))]
    {
        let before = test_support::allocation::live_bytes();
        let (profile, allocation) = test_support::allocation::measure(FacetWorkload::new);
        eprintln!(
            "facet-layout-memory fixture=synthetic_10k_sparse retained_delta_bytes={} peak_delta_bytes={} allocation_calls={} reallocation_calls={}",
            test_support::allocation::live_bytes() - before,
            allocation.peak_bytes,
            allocation.allocation_calls,
            allocation.reallocation_calls,
        );
        black_box(profile.scene.world.object_count());
    }
    let mut update = FacetWorkload::new();
    group.throughput(Throughput::Elements(update.operations_per_cycle() as u64));
    group.bench_function("mixed_update_cycle/synthetic_10k", |b| {
        b.iter(|| black_box(update.cycle()));
    });

    let read = FacetWorkload::new();
    group.throughput(Throughput::Elements((LAYOUT_OBJECTS * 2) as u64));
    group.bench_function("list_and_detail_read/synthetic_10k", |b| {
        b.iter(|| black_box(read.read()));
    });

    group.throughput(Throughput::Elements(257));
    group.bench_function("bounded_cleanup/synthetic_10k_257_linkset", |b| {
        b.iter_batched_ref(
            || {
                let mut workload = FacetWorkload::new();
                let root = workload.scene.rows[100];
                for index in 1..=256 {
                    let child = workload.scene.rows[100 + index];
                    let mut patch = ObjectPatch::new(source(child.region, workload.order), child.local_id);
                    workload.order += 1;
                    patch.parent = Some(Local(root.local_id));
                    let _ = workload.scene.world.patch_object(patch).expect("link layout cleanup fixture");
                }
                workload
            },
            |workload| {
                let root = workload.scene.rows[100];
                let _ = black_box(
                    workload
                        .scene
                        .world
                        .remove_object(source(root.region, workload.order), root.local_id)
                        .expect("remove layout cleanup root"),
                );
                let mut turns = 0;
                let mut total_work = 0;
                let mut maximum_work = 0;
                while workload.scene.world.has_pending_removals() {
                    let progress = workload.scene.world.reclaim_removals(MAX_REMOVAL_WORK_PER_TURN);
                    assert!(progress.work_units <= MAX_REMOVAL_WORK_PER_TURN);
                    assert!(progress.work_units > 0 || !progress.pending);
                    turns += 1;
                    total_work += progress.work_units;
                    maximum_work = maximum_work.max(progress.work_units);
                    drop(progress);
                }
                assert_eq!(workload.scene.world.object_count(), LAYOUT_OBJECTS - 257);
                black_box((turns, total_work, maximum_work));
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn bench_avatar_updates(group: &mut BenchmarkGroup<'_, WallTime>) {
    let mut scene = populated_scene_with_avatars(10_000, 11, false);
    let mut order = INITIAL_ORDER + PER_REGION as u64 + 1;
    let param = AvatarVisualParamIndex::new(0).unwrap();
    for row in &scene.rows[1..=10] {
        let mut patch = ObjectPatch::new(source(row.region, order), row.local_id);
        order += 1;
        patch.avatar_appearance =
            Some(AvatarAppearanceUpdate::new(None, None, vec![(param, 0)], Some(AvatarTextureSlots::default())).unwrap());
        let _ = scene.world.patch_object(patch).expect("avatar texture table");
    }
    let mut weight = 0_u8;
    group.throughput(Throughput::Elements(10));
    group.bench_function("avatar_visual_params/10_avatars", |b| {
        b.iter(|| {
            weight = weight.wrapping_add(1);
            for row in &scene.rows[1..=10] {
                let mut patch = ObjectPatch::new(source(row.region, order), row.local_id);
                order += 1;
                patch.avatar_appearance = Some(AvatarAppearanceUpdate::new(None, None, vec![(param, weight)], None).unwrap());
                let _ = black_box(scene.world.patch_object(patch).expect("avatar parameter update"));
            }
        });
    });
    let avatar = scene.world.entity(scene.rows[1].id).unwrap();
    assert_eq!(scene.world.object(avatar).unwrap().avatar_appearance().unwrap().visual_param(param), Some(weight));
}

fn bench_transform(group: &mut BenchmarkGroup<'_, WallTime>) {
    for &count in SCENE_SIZES {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("transform", size_name(count)), &count, |b, &count| {
            let mut scene = populated_scene(count, false, false);
            let mut order = INITIAL_ORDER + PER_REGION as u64 + 1;
            let mut alternate = false;
            b.iter(|| {
                let pose = if alternate {
                    Pose { position: Some(Vec3::splat(2.0)), rotation: Some(Quat::IDENTITY) }
                } else {
                    Pose { position: Some(Vec3::ONE), rotation: Some(Quat::from_rotation_y(0.25)) }
                };
                alternate = !alternate;
                for row in &scene.rows {
                    let mut patch = ObjectPatch::new(source(row.region, order), row.local_id);
                    order += 1;
                    patch.pose = Some(pose);
                    let _ = black_box(scene.world.patch_object(patch).expect("transform update"));
                }
            });
        });
    }
}

fn bench_observation(group: &mut BenchmarkGroup<'_, WallTime>) {
    let mut scene = populated_scene(10_000, false, false);
    let mut order = INITIAL_ORDER + PER_REGION as u64 + 1;
    let mut alternate = false;
    let mut expected_position = Vec3::ZERO;
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("observation/10k", |b| {
        b.iter(|| {
            let position = if alternate { Vec3::ONE } else { Vec3::splat(2.0) };
            expected_position = position;
            alternate = !alternate;
            for row in &scene.rows {
                let observation =
                    observation_with_pose(Row { order, ..*row }, Pose { position: Some(position), rotation: Some(Quat::IDENTITY) });
                order += 1;
                let _ = black_box(scene.world.observe_object(observation).expect("fresh observation"));
            }
        });
    });
    for row in &scene.rows {
        let entity = scene.world.entity(row.id).expect("observed object remains");
        assert_eq!(scene.world.object(entity).unwrap().position(), Some(expected_position));
    }
}

fn bench_duplicate(group: &mut BenchmarkGroup<'_, WallTime>) {
    for &count in SCENE_SIZES {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("duplicate", size_name(count)), &count, |b, &count| {
            let mut scene = populated_scene(count, false, false);
            for row in &scene.rows {
                let applied = scene.world.observe_object(observation(*row)).expect("duplicate observation");
                assert_eq!(applied.outcome, IgnoredStale);
            }
            b.iter(|| {
                for row in &scene.rows {
                    let _ = black_box(scene.world.observe_object(observation(*row)).expect("duplicate observation"));
                }
            });
        });
    }
}

fn bench_stale(group: &mut BenchmarkGroup<'_, WallTime>) {
    for &count in SCENE_SIZES {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("stale", size_name(count)), &count, |b, &count| {
            let mut scene = populated_scene(count, false, false);
            for row in &scene.rows {
                let mut patch = ObjectPatch::new(source(row.region, row.order - 1), row.local_id);
                patch.pose = Some(Pose { position: Some(Vec3::ONE), rotation: Some(Quat::IDENTITY) });
                let applied = scene.world.patch_object(patch).expect("stale update");
                assert_eq!(applied.outcome, Unchanged);
            }
            b.iter(|| {
                for row in &scene.rows {
                    let mut patch = ObjectPatch::new(source(row.region, row.order - 1), row.local_id);
                    patch.pose = Some(Pose { position: Some(Vec3::ONE), rotation: Some(Quat::IDENTITY) });
                    let _ = black_box(scene.world.patch_object(patch).expect("stale update"));
                }
            });
        });
    }
}

fn bench_mixed(group: &mut BenchmarkGroup<'_, WallTime>) {
    for &count in SCENE_SIZES {
        let recycle_count = count.div_ceil(16);
        group.throughput(Throughput::Elements((count * 5 + recycle_count * 2) as u64));
        group.bench_with_input(BenchmarkId::new("mixed", size_name(count)), &count, |b, &count| {
            let mut scene = populated_scene(count, false, false);
            let mut base_order = 200_000_u64;
            let mut next_id = count as u128 + 1;
            let mut round = 0_u64;
            let mut duplicate_pose = Pose { position: Some(Vec3::ZERO), rotation: Some(Quat::IDENTITY) };
            for row in &scene.rows {
                let applied = scene.world.observe_object(observation(*row)).expect("duplicate observation");
                assert_eq!(applied.outcome, IgnoredStale);
            }
            b.iter(|| {
                let alternate = round % 2 == 1;
                let position = if alternate { Vec3::splat(2.0) } else { Vec3::ONE };
                let rotation = if alternate { Quat::IDENTITY } else { Quat::from_rotation_y(0.25) };
                let pose = Pose { position: Some(position), rotation: Some(rotation) };
                let (velocity, acceleration) = if alternate { (Vec3::Y, Vec3::Z) } else { (Vec3::X, Vec3::Y) };
                for row in &scene.rows {
                    let _ =
                        black_box(scene.world.observe_object(observation_with_pose(*row, duplicate_pose)).expect("duplicate observation"));
                }
                for row in &mut scene.rows {
                    let order = base_order + u64::from(row.local_id.get());
                    let mut patch = ObjectPatch::new(source(row.region, order), row.local_id);
                    patch.parent = Some(Root);
                    patch.pose = Some(pose);
                    patch.scale = Some(Vec3::ONE);
                    let _ = black_box(scene.world.patch_object(patch).expect("transform update"));
                    row.order = order;
                }
                for row in &scene.rows {
                    let mut patch = ObjectPatch::new(source(row.region, row.order), row.local_id);
                    patch.linear_motion = LinearMotion { velocity: Some(velocity), acceleration: Some(acceleration) };
                    let _ = black_box(scene.world.patch_object(patch).expect("motion update"));
                }
                for row in &scene.rows {
                    let mut patch = ObjectPatch::new(source(row.region, row.order - 1), row.local_id);
                    patch.pose = Some(Pose { position: Some(Vec3::splat(3.0)), rotation: Some(Quat::IDENTITY) });
                    let _ = black_box(scene.world.patch_object(patch).expect("stale update"));
                }
                for index in (0..scene.rows.len()).step_by(16) {
                    let row = scene.rows[index];
                    let remove_order = base_order + 300_000 + index as u64 * 2;
                    let _ = black_box(scene.world.remove_object(source(row.region, remove_order), row.local_id).expect("object removal"));
                    let replacement = Row { id: Uuid::from_u128(next_id), order: remove_order + 1, ..row };
                    next_id += 1;
                    let _ = black_box(scene.world.observe_object(observation_with_pose(replacement, pose)).expect("local ID reuse"));
                    scene.rows[index] = replacement;
                }
                duplicate_pose = pose;
                base_order += 500_000;
                round += 1;
            });
        });
    }
}

fn content_scene() -> Scene {
    let mut scene = populated_scene(10_000, false, false);
    let surface = SurfaceAppearance::new(
        FaceAppearance { texture: Some(AssetId::new(Uuid::from_u128(99))), ..FaceAppearance::default() },
        vec![(1, FaceAppearance::default())],
    )
    .expect("surface appearance");
    let materials = PbrMaterialAssignments::new(vec![(1, AssetId::new(Uuid::from_u128(100)))]).expect("material bindings");
    let overrides = PbrMaterialOverrides::new(vec![(1, PbrMaterialOverride { roughness: Some(0.5), ..PbrMaterialOverride::default() })])
        .expect("material overrides");
    for row in &scene.rows {
        let mut patch = ObjectPatch::new(source(row.region, row.order), row.local_id);
        patch.appearance = FieldUpdate::Set(Appearance::Surface);
        patch.surface_appearance = Some(surface.clone());
        patch.pbr_materials = Some(materials.clone());
        let _ = scene.world.patch_object(patch).expect("surface content");
        let mut patch = ObjectPatch::new(source(row.region, row.order), row.local_id);
        patch.pbr_material_overrides = Some(overrides.clone());
        let _ = scene.world.patch_object(patch).expect("material overrides");
    }
    scene
}

fn bench_content_updates(group: &mut BenchmarkGroup<'_, WallTime>) {
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("stale_with_content/10k", |b| {
        let mut scene = content_scene();
        b.iter(|| {
            for row in &scene.rows {
                let mut patch = ObjectPatch::new(source(row.region, row.order - 1), row.local_id);
                patch.pose = Some(Pose { position: Some(Vec3::ONE), rotation: None });
                let applied = scene.world.patch_object(patch).expect("stale patch");
                assert_eq!(applied.outcome, Unchanged);
                let _ = black_box(applied);
            }
        });
    });
    group.bench_function("transform_with_content/10k", |b| {
        let mut scene = content_scene();
        let mut order = INITIAL_ORDER + PER_REGION as u64 + 1;
        let mut alternate = false;
        b.iter(|| {
            let position = if alternate { Vec3::ONE } else { Vec3::splat(2.0) };
            alternate = !alternate;
            for row in &scene.rows {
                let mut patch = ObjectPatch::new(source(row.region, order), row.local_id);
                order += 1;
                patch.pose = Some(Pose { position: Some(position), rotation: None });
                let _ = black_box(scene.world.patch_object(patch).expect("transform patch"));
            }
        });
    });
}
