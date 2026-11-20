use std::{hint::black_box, thread, time::Instant};

use assets::{Budget, DecodeJob, Demand, LoadJob, Priority, Request, Store, TextureSize};
use divan::counter::ItemsCount;
use test_support::texture::GRAY_4096_J2C;
use utils::AssetId;
use uuid::Uuid;

fn decode_jobs(edge: u32, workers: usize) -> (Store, Vec<DecodeJob>, Vec<Demand>, Instant) {
    let now = Instant::now();
    let mut store = Store::new(Budget::default());
    let route = store.register_route().expect("benchmark route");
    let mut demands = Vec::with_capacity(workers);
    for index in 0..workers {
        let id = AssetId::new(Uuid::from_u128(index as u128 + 1));
        demands.push(
            store.request(route, Request::Texture { id, size: TextureSize::new(edge) }, Priority::Visible, now).expect("benchmark demand"),
        );
    }
    let mut jobs = Vec::with_capacity(workers);
    for _ in 0..workers {
        let LoadJob::Fetch(fetch) = store.next_job(now).expect("benchmark fetch") else {
            panic!("new texture request unexpectedly used a cached source");
        };
        let input = fetch.complete(Ok(GRAY_4096_J2C.to_vec())).expect("fixture response");
        let job = store.start_decode(input).unwrap_or_else(|_| panic!("decode admission available"));
        jobs.push(job);
    }
    (store, jobs, demands, now)
}

#[derive(Clone, Copy, Debug)]
struct DecodeCase {
    edge: u32,
    workers: usize,
}

const ONE_WORKER: &[DecodeCase] =
    &[DecodeCase { edge: 512, workers: 1 }, DecodeCase { edge: 1024, workers: 1 }, DecodeCase { edge: 2048, workers: 1 }];

const TWO_WORKERS: &[DecodeCase] =
    &[DecodeCase { edge: 512, workers: 2 }, DecodeCase { edge: 1024, workers: 2 }, DecodeCase { edge: 2048, workers: 2 }];

mod benches {
    use super::*;

    fn run(bencher: divan::Bencher, case: DecodeCase) {
        bencher
            .counter(ItemsCount::new(u64::from(case.edge).pow(2) * case.workers as u64))
            .with_inputs(|| decode_jobs(case.edge, case.workers))
            .bench_local_values(|(mut store, jobs, demands, now)| {
                let completions = thread::scope(|scope| {
                    jobs.into_iter()
                        .map(|job| scope.spawn(move || job.run()))
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|job| job.join().expect("decode worker completes"))
                        .collect::<Vec<_>>()
                });
                for completion in completions {
                    store.complete(completion, now);
                }
                for demand in demands {
                    black_box(demand.status());
                }
            });
    }

    #[divan::bench(args = ONE_WORKER)]
    fn texture_assets_decode_gray_4096_one_worker(bencher: divan::Bencher, case: DecodeCase) {
        run(bencher, case);
    }

    #[divan::bench(args = TWO_WORKERS)]
    fn texture_assets_decode_gray_4096_two_workers(bencher: divan::Bencher, case: DecodeCase) {
        run(bencher, case);
    }
}
