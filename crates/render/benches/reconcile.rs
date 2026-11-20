use std::hint;

use divan::counter::ItemsCount;
use render::scene::DrawnMeshKey;

use crate::support;

const OBJECTS: usize = 10_000;
const REGIONS: usize = 4;
const MESH_COUNTS: &[usize] = &[16, 128, 1024];

mod initial {
    use super::*;

    #[divan::bench]
    fn scene_reconcile_initial_first_full_view_batch_10k_4_regions(bencher: divan::Bencher) {
        bencher.with_inputs(|| support::scene_world(OBJECTS, REGIONS)).bench_local_values(|(world, _)| {
            support::first_full_view_batch(&world, OBJECTS);
            hint::black_box(world.object_count());
        });
    }
}

mod diagnostic {
    use super::*;

    #[divan::bench]
    fn scene_diagnostic_count_tracked_scene_records_10k_4_regions(bencher: divan::Bencher) {
        let (world, _) = support::scene_world(OBJECTS, REGIONS);
        let (_reconciler, view, _now) = support::settle(&world, OBJECTS);
        bencher.bench_local(|| hint::black_box(&view).tracked_object_count());
    }
}

mod incremental {
    use super::*;

    #[divan::bench]
    fn scene_reconcile_incremental_pose_1_percent_10k_4_regions(bencher: divan::Bencher) {
        let (mut world, rows) = support::scene_world(OBJECTS, REGIONS);
        let (mut reconciler, mut view, mut now) = support::settle(&world, OBJECTS);
        let mut order = 2_000_000;
        let mut generation = 1;
        bencher.bench_local(|| {
            support::update_one_percent(&mut world, &rows, &mut order, generation);
            support::first_publish(&mut reconciler, &mut view, &world, &mut now);
            generation += 1;
            hint::black_box(view.object_count());
        });
    }
}

mod mesh_lod {
    use super::*;

    #[divan::bench(args = MESH_COUNTS)]
    fn scene_mesh_lod_lookup_camera_change_distinct_assets(bencher: divan::Bencher, count: usize) {
        bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| support::ready_distinct_mesh_scene(count)).bench_local_values(
            |(mut reconciler, mut view, world, mut now)| {
                let work = support::request_distinct_mesh_refinement(&mut reconciler, &mut view, &world, &mut now);
                hint::black_box((
                    work.requested.len(),
                    work.released.len(),
                    view.objects().filter(|object| matches!(object.mesh().map(|(key, _)| key), Some(DrawnMeshKey::Asset { .. }))).count(),
                ));
            },
        );
    }
}
