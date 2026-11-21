use std::{
    assert_matches,
    hint::black_box,
    mem,
    time::{Duration, Instant},
};

use assets::{
    Budget, Demand, Failure, Kind, LoadJob, MAX_NON_MESH_FETCHES, MAX_TEXTURE_DECODES, Priority, Request, Resource, Route, Store,
    TextureSize,
};
use divan::counter::ItemsCount;
use test_support::texture::GRAY_4096_J2C;
use utils::AssetId;
use uuid::Uuid;

type Fixture = (Store, Vec<Demand>, Vec<Route>, Instant, Vec<assets::DecodeJob>);
const DIRTY_BACKLOG: usize = 512;

fn fixture(count: usize) -> Fixture {
    let now = Instant::now();
    let mut store = Store::new(Budget::default());
    let routes = (0..4).map(|_| store.register_route().expect("benchmark route")).collect::<Vec<_>>();
    let mut demands = Vec::with_capacity(count + MAX_TEXTURE_DECODES * 2);
    let mut decodes = Vec::with_capacity(MAX_TEXTURE_DECODES);

    for (index, route) in routes.iter().enumerate().take(MAX_TEXTURE_DECODES) {
        let id = AssetId::new(Uuid::from_u128(u128::MAX - index as u128));
        let low =
            store.request(*route, Request::Texture { id, size: TextureSize::new(8) }, Priority::Visible, now).expect("benchmark demand");
        let LoadJob::Fetch(fetch) = store.next_job(now).expect("benchmark warm fetch") else {
            panic!("warm texture unexpectedly used a cached source");
        };
        let input = fetch.complete(Ok(GRAY_4096_J2C.to_vec())).expect("fixture response");
        let decode = store.start_decode(input).expect("benchmark warm decode");
        store.complete(decode.run(), now);
        demands.push(low);

        demands.push(
            store
                .request(*route, Request::Texture { id, size: TextureSize::new(64) }, Priority::Visible, now)
                .expect("benchmark refinement demand"),
        );
        let LoadJob::Decode(input) = store.next_job(now).expect("benchmark cached decode") else {
            panic!("warm texture refinement unexpectedly fetched");
        };
        decodes.push(store.start_decode(input).expect("benchmark cached decode admission"));
    }

    for index in 0..count {
        let id = AssetId::new(Uuid::from_u128(index as u128 + 1));
        let resource = Resource {
            id,
            kind: match index % 4 {
                0 => Kind::Texture,
                1 => Kind::PbrMaterial,
                2 => Kind::LegacyMaterial,
                _ => Kind::Environment,
            },
        };
        let priority = if index < MAX_NON_MESH_FETCHES / 2 || index % 7 == 0 { Priority::Visible } else { Priority::Background };
        demands.push(store.request(routes[index % routes.len()], Request::new(resource), priority, now).expect("benchmark demand"));
    }

    for _ in 0..MAX_NON_MESH_FETCHES / 2 {
        let attempts = count.div_ceil(128).saturating_add(4);
        let job = (0..attempts).find_map(|_| store.next_job(now)).expect("benchmark retry seed");
        store.complete(job.fail(Failure::Transient { retry_after: Duration::from_secs(20) }), now);
    }

    (store, demands, routes, now, decodes)
}

fn saturated_fixture(count: usize) -> (Fixture, Vec<LoadJob>) {
    let mut fixture = fixture(count);
    let mut jobs = Vec::with_capacity(MAX_NON_MESH_FETCHES);
    let turns = count.div_ceil(128).saturating_add(4);
    for _ in 0..turns {
        while jobs.len() < MAX_NON_MESH_FETCHES {
            let Some(job) = fixture.0.next_job(fixture.3) else { break };
            jobs.push(job);
        }
        if jobs.len() == MAX_NON_MESH_FETCHES {
            break;
        }
    }
    (fixture, jobs)
}

fn settle(fixture: &mut Fixture, count: usize) {
    maintain_until_idle(&mut fixture.0, fixture.3, count.div_ceil(128).saturating_add(2));
}

fn maintain_until_idle(store: &mut Store, now: Instant, max_turns: usize) -> usize {
    let mut turns = 0;
    loop {
        turns += 1;
        if !store.maintain(now) {
            return turns;
        }
        assert!(turns < max_turns, "benchmark maintenance remains bounded");
    }
}

fn dirty_progress_fixture(count: usize) -> (Fixture, Vec<Demand>, Vec<Demand>) {
    let mut fixture = fixture(count);
    settle(&mut fixture, count);
    let eligible: Vec<_> = (0..MAX_NON_MESH_FETCHES / 2)
        .map(|index| {
            let resource = Resource { id: AssetId::new(Uuid::from_u128(u128::MAX / 2 + index as u128)), kind: Kind::Texture };
            fixture.0.request(fixture.2[0], Request::new(resource), Priority::Visible, fixture.3).expect("benchmark eligible demand")
        })
        .collect();
    let churn: Vec<_> = (0..DIRTY_BACKLOG)
        .map(|index| {
            let resource = Resource { id: AssetId::new(Uuid::from_u128(u128::MAX / 3 + index as u128)), kind: Kind::Texture };
            fixture
                .0
                .request_with_importance(fixture.2[0], Request::new(resource), Priority::Visible, u16::MAX, fixture.3)
                .expect("benchmark churn demand")
        })
        .collect();
    settle(&mut fixture, count + eligible.len() + churn.len());
    (fixture, eligible, churn)
}

const RECONCILE_COUNTS: &[usize] = &[1_000, 65_000];
const STORE_COUNTS: &[usize] = &[1_000, 10_000, 50_000];

mod demand_reconcile {
    use super::*;

    #[divan::bench(args = RECONCILE_COUNTS)]
    fn store_demand_reconcile_turn(bencher: divan::Bencher, count: usize) {
        bencher
            .counter(ItemsCount::new(count as u64))
            .with_inputs(|| {
                let mut state = fixture(count);
                settle(&mut state, count);
                state.1.last().expect("benchmark demand").update_priority(Priority::Visible);
                state
            })
            .bench_local_refs(|(store, demands, _, now, decodes)| {
                store.maintain(*now);
                black_box((demands.len(), decodes.len()));
            });
    }

    #[divan::bench(args = RECONCILE_COUNTS)]
    fn store_demand_reconcile_admission_during_churn(bencher: divan::Bencher, count: usize) {
        bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| dirty_progress_fixture(count)).bench_local_refs(
            |((store, _, _, now, _), eligible, churn)| {
                let mut turns = 0;
                let job = loop {
                    for demand in churn.iter() {
                        demand.update_priority(Priority::Visible);
                        demand.update_priority(Priority::Background);
                    }
                    turns += 1;
                    if let Some(job) = store.next_job(*now) {
                        break job;
                    }
                    assert!(turns < 8, "bounded turns admit work during sustained dirty churn");
                };
                assert_matches!(&job, LoadJob::Fetch(_), "admission includes a network fetch");
                assert!(store.maintain(*now), "dirty work remains after useful admission");
                black_box((turns, eligible.len()));
            },
        );
    }

    #[divan::bench(args = RECONCILE_COUNTS)]
    fn store_demand_reconcile_zero_interest_drain(bencher: divan::Bencher, count: usize) {
        bencher
            .counter(ItemsCount::new(count as u64))
            .with_inputs(|| {
                let mut state = fixture(count);
                settle(&mut state, count);
                state
            })
            .bench_local_refs(|(store, demands, _, now, decodes)| {
                drop(mem::take(demands));
                drop(mem::take(decodes));
                black_box(maintain_until_idle(store, *now, count.div_ceil(128).saturating_add(2)));
            });
    }

    #[divan::bench(args = RECONCILE_COUNTS)]
    fn store_demand_reconcile_zero_interest_turn(bencher: divan::Bencher, count: usize) {
        bencher
            .counter(ItemsCount::new(count as u64))
            .with_inputs(|| {
                let mut state = fixture(count);
                settle(&mut state, count);
                drop(mem::take(&mut state.1));
                drop(mem::take(&mut state.4));
                state
            })
            .bench_local_refs(|(store, _, _, now, _)| black_box(store.maintain(*now)));
    }

    #[divan::bench(args = RECONCILE_COUNTS)]
    fn store_demand_reconcile_priority_churn_drain(bencher: divan::Bencher, count: usize) {
        bencher
            .counter(ItemsCount::new(count as u64))
            .with_inputs(|| {
                let (mut state, jobs) = saturated_fixture(count);
                settle(&mut state, count);
                (state, jobs)
            })
            .bench_local_refs(|((store, demands, _, now, decodes), jobs)| {
                for demand in demands.iter() {
                    demand.update_priority(Priority::Visible);
                    demand.update_priority(Priority::Background);
                }
                let turns = maintain_until_idle(store, *now, count.div_ceil(128).saturating_add(2));
                black_box((turns, decodes.len(), jobs.len()));
            });
    }

    #[divan::bench(args = RECONCILE_COUNTS)]
    fn store_demand_reconcile_priority_churn_turn(bencher: divan::Bencher, count: usize) {
        bencher
            .counter(ItemsCount::new(count as u64))
            .with_inputs(|| {
                let (mut state, jobs) = saturated_fixture(count);
                settle(&mut state, count);
                for demand in &state.1 {
                    demand.update_priority(Priority::Visible);
                    demand.update_priority(Priority::Background);
                }
                (state, jobs)
            })
            .bench_local_refs(|((store, _, _, now, decodes), jobs)| black_box((store.maintain(*now), decodes.len(), jobs.len())));
    }
}

#[divan::bench(args = STORE_COUNTS)]
fn store_drain(bencher: divan::Bencher, count: usize) {
    bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| fixture(count)).bench_local_refs(|(store, demands, _, now, decodes)| {
        let mut jobs = Vec::with_capacity(MAX_NON_MESH_FETCHES);
        loop {
            if let Some(job) = store.next_job(*now) {
                jobs.push(job);
            } else if !store.maintain(*now) {
                break;
            }
        }
        for decode in mem::take(decodes) {
            store.complete(decode.fail(Failure::NotFound), *now);
        }
        let completed = jobs.len();
        for job in jobs {
            match job {
                LoadJob::Fetch(fetch) => drop(store.complete(LoadJob::Fetch(fetch).fail(Failure::NotFound), *now)),
                LoadJob::Decode(input) => {
                    let decode = store.start_decode(input).expect("benchmark decode admission");
                    drop(store.complete(decode.fail(Failure::NotFound), *now));
                }
                LoadJob::ReadCache(_) => unreachable!("benchmark Store has no persistent cache"),
            }
        }
        black_box((completed, demands.len()));
    });
}

#[divan::bench(args = STORE_COUNTS)]
fn store_saturated_next_job(bencher: divan::Bencher, count: usize) {
    bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| saturated_fixture(count)).bench_local_refs(
        |((store, demands, _, now, decodes), jobs)| {
            let next_job = store.next_job(*now);
            black_box((next_job, demands.len(), decodes.len(), jobs.len()));
        },
    );
}

#[divan::bench(args = STORE_COUNTS)]
fn store_maintain(bencher: divan::Bencher, count: usize) {
    bencher.counter(ItemsCount::new(count as u64)).with_inputs(|| fixture(count)).bench_local_refs(|(store, demands, _, now, decodes)| {
        store.maintain(*now);
        black_box((demands.len(), decodes.len()));
    });
}

#[divan::bench(args = STORE_COUNTS)]
fn store_request(bencher: divan::Bencher, count: usize) {
    let (mut store, _demands, routes, now, _decodes) = fixture(count);
    let resource = Resource { id: AssetId::new(Uuid::from_u128(1)), kind: Kind::Texture };
    bencher.bench_local(|| {
        let demand = store.request(routes[1], Request::new(resource), Priority::Visible, now).expect("benchmark route admission");
        drop(black_box(demand));
    });
}
