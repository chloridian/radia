use std::{hint::black_box, sync::Arc, time::Instant};

use assets::{Budget, Kind, LoadJob, Priority, Request, Resource, Store, TextureSize, cache::AssetCache};
use tempfile::tempdir;
use test_support::texture::{GRAY_4096_J2C, RGBA_J2C};
use utils::AssetId;
use uuid::Uuid;

fn resource() -> Resource {
    Resource { id: AssetId::new(Uuid::from_u128(1)), kind: Kind::Texture }
}

#[divan::bench]
fn asset_cache_texture_cold_fetch_decode(bencher: divan::Bencher) {
    bencher
        .with_inputs(|| {
            let mut store = Store::new(Budget::default());
            let route = store.register_route().expect("register benchmark route");
            (store, route, RGBA_J2C.to_vec())
        })
        .bench_local_values(|(mut store, route, payload)| {
            let now = Instant::now();
            let _demand = store.request(route, Request::new(resource()), Priority::Visible, now).expect("request benchmark texture");
            let Some(LoadJob::Fetch(fetch)) = store.next_job(now) else { panic!("cold texture starts with fetch") };
            let input = fetch.complete(Ok(payload)).expect("attach benchmark response");
            let decode = store.start_decode(input).expect("admit benchmark decode");
            black_box(store.complete(decode.run(), now));
        });
}

#[divan::bench]
fn asset_cache_texture_warm_disk_read_decode(bencher: divan::Bencher) {
    let directory = tempdir().expect("benchmark cache directory");
    let cache = Arc::new(AssetCache::open(directory.path(), 1024 * 1024 * 1024).expect("open benchmark cache"));
    cache.put_complete(Request::new(resource()), RGBA_J2C.to_vec()).expect("seed benchmark cache");

    bencher
        .with_inputs(|| {
            let mut store = Store::with_cache(Budget::default(), Arc::clone(&cache));
            let route = store.register_route().expect("register benchmark route");
            (store, route)
        })
        .bench_local_values(|(mut store, route)| {
            let now = Instant::now();
            let _demand = store.request(route, Request::new(resource()), Priority::Visible, now).expect("request benchmark texture");
            let Some(LoadJob::ReadCache(read)) = store.next_job(now) else { panic!("warm texture starts with disk read") };
            let Some(LoadJob::Decode(input)) = store.complete_cache_read(read.run(), now) else {
                panic!("disk hit supplies decoder input")
            };
            let decode = store.start_decode(input).expect("admit benchmark decode");
            black_box(store.complete(decode.run(), now));
        });

    Arc::try_unwrap(cache).expect("benchmark cache handle released").shutdown().expect("close benchmark cache");
}

#[divan::bench]
fn asset_cache_texture_warm_512_prepared_hit(bencher: divan::Bencher) {
    let directory = tempdir().expect("benchmark cache directory");
    let cache = Arc::new(AssetCache::open(directory.path(), 1024 * 1024 * 1024).expect("open benchmark cache"));
    let request = Request::Texture { id: AssetId::new(Uuid::from_u128(2)), size: TextureSize::new(512) };
    cache.put_complete(request, GRAY_4096_J2C.to_vec()).expect("seed benchmark cache");

    {
        let mut store = Store::with_cache(Budget::default(), Arc::clone(&cache));
        let route = store.register_route().expect("register seed route");
        let now = Instant::now();
        let _demand = store.request(route, request, Priority::Visible, now).expect("seed demand");
        let Some(LoadJob::ReadCache(read)) = store.next_job(now) else { panic!("seed warm read") };
        let Some(LoadJob::Decode(input)) = store.complete_cache_read(read.run(), now) else { panic!("seed decode") };
        let decode = store.start_decode(input).expect("seed start decode");
        for write in store.complete(decode.run(), now) {
            write.run();
        }
    }

    bencher
        .with_inputs(|| {
            let mut store = Store::with_cache(Budget::default(), Arc::clone(&cache));
            let route = store.register_route().expect("register benchmark route");
            (store, route)
        })
        .bench_local_values(|(mut store, route)| {
            let now = Instant::now();
            let _demand = store.request(route, request, Priority::Visible, now).expect("request benchmark texture");
            let Some(LoadJob::ReadCache(read)) = store.next_job(now) else { panic!("warm texture starts with disk read") };
            let Some(LoadJob::Decode(input)) = store.complete_cache_read(read.run(), now) else {
                panic!("disk hit supplies decoder input")
            };
            let decode = store.start_decode(input).expect("admit benchmark decode");
            black_box(store.complete(decode.run(), now));
        });

    Arc::try_unwrap(cache).expect("benchmark cache handle released").shutdown().expect("close benchmark cache");
}
