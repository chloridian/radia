use std::{
    array, assert_matches,
    hint::black_box,
    sync::{Arc, Barrier, mpsc},
    thread,
};

use divan::counter::ItemsCount;
use storage::{Cache, DEFAULT_SIZE_LIMIT, Key, Namespace, NamespaceId, StorageError, UnavailableReason};
use tempfile::tempdir;

const EXISTING_ENTRIES: u64 = 50_000;
const BURST_WRITES: usize = 16;
const SETUP_WRITERS: usize = 32;
const PAYLOAD_BYTES: usize = 64;
const NAMESPACES: [Namespace; 1] = [Namespace::new("bench", "bin")];

struct CacheGuard(Option<Cache>);

impl Drop for CacheGuard {
    fn drop(&mut self) {
        if let Some(cache) = self.0.take() {
            cache.shutdown().expect("close replay fixture");
        }
    }
}

fn key(value: u64) -> Key {
    let mut name = [0; 16];
    name[..8].copy_from_slice(&value.to_le_bytes());
    Key::new(NamespaceId::new(0), name)
}

fn put_until_accepted(cache: &Cache, key: Key) {
    loop {
        match cache.put(key, vec![0x5a; PAYLOAD_BYTES]) {
            Ok(()) => return,
            Err(StorageError::Unavailable(UnavailableReason::WriteQueueFull)) => thread::yield_now(),
            Err(error) => panic!("benchmark put failed: {error}"),
        }
    }
}

fn populate(cache: &Cache) {
    let barrier = Arc::new(Barrier::new(SETUP_WRITERS));
    thread::scope(|scope| {
        for worker in 0..SETUP_WRITERS {
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                for value in (u64::try_from(worker).expect("worker index fits")..EXISTING_ENTRIES).step_by(SETUP_WRITERS) {
                    put_until_accepted(cache, key(value));
                }
            });
        }
    });
    cache.flush().expect("compact benchmark fixture");
}

fn populated_cache() -> (tempfile::TempDir, Cache, Key) {
    let directory = tempdir().expect("create benchmark directory");
    let cache = Cache::open(directory.path(), DEFAULT_SIZE_LIMIT, &NAMESPACES).expect("open benchmark cache");
    populate(&cache);
    assert_eq!(cache.entry_count(), usize::try_from(EXISTING_ENTRIES).expect("entry count fits"));
    let hit_key = key(EXISTING_ENTRIES / 2);
    assert_matches!(cache.lookup(hit_key), storage::Lookup::Hit(_));
    (directory, cache, hit_key)
}

mod scale {
    use super::*;

    #[divan::bench]
    fn scale_put_single(bencher: divan::Bencher) {
        let (_directory, cache, _) = populated_cache();
        let item_key = key(EXISTING_ENTRIES);

        bencher
            .counter(ItemsCount::new(1_u64))
            .with_inputs(|| cache.remove(item_key).expect("reset single insertion"))
            .bench_local_values(|_| cache.put(item_key, vec![0x5a; PAYLOAD_BYTES]).expect("single put"));

        assert!(cache.remove(item_key).expect("remove inserted entry"));
        assert_eq!(cache.entry_count(), EXISTING_ENTRIES as usize);
        cache.shutdown().expect("close benchmark cache");
    }

    #[divan::bench]
    fn scale_put_burst_16(bencher: divan::Bencher) {
        let (_directory, cache, _) = populated_cache();
        let cache_ref = &cache;
        let keys: [Key; BURST_WRITES] = array::from_fn(|index| key(EXISTING_ENTRIES + index as u64));

        thread::scope(|scope| {
            let start = Arc::new(Barrier::new(BURST_WRITES + 1));
            let producers: Vec<_> = keys
                .iter()
                .copied()
                .map(|item_key| {
                    let (sender, receiver) = mpsc::sync_channel(0);
                    let (completed, completion) = mpsc::channel();
                    let start = Arc::clone(&start);
                    scope.spawn(move || {
                        while receiver.recv().is_ok() {
                            start.wait();
                            put_until_accepted(cache_ref, item_key);
                            completed.send(()).expect("report burst completion");
                        }
                    });
                    (sender, completion)
                })
                .collect();

            bencher
                .counter(ItemsCount::new(BURST_WRITES as u64))
                .with_inputs(|| {
                    for item_key in keys {
                        cache_ref.remove(item_key).expect("reset burst insertion");
                    }
                })
                .bench_local_values(|_| {
                    for (producer, _) in &producers {
                        producer.send(()).expect("start burst insertion");
                    }
                    start.wait();
                    for (_, completion) in &producers {
                        completion.recv().expect("burst insertion completed");
                    }
                });
        });

        for item_key in keys {
            assert!(cache.remove(item_key).expect("remove burst entry"));
        }
        assert_eq!(cache.entry_count(), EXISTING_ENTRIES as usize);
        cache.shutdown().expect("close benchmark cache");
    }

    #[divan::bench]
    fn scale_lookup_hit(bencher: divan::Bencher) {
        let (_directory, cache, hit_key) = populated_cache();
        bencher.counter(ItemsCount::new(1_u64)).bench_local(|| black_box(cache.lookup(hit_key)));
        cache.shutdown().expect("close benchmark cache");
    }
}

mod open_replay {
    use super::*;

    #[divan::bench]
    fn scale_open_replay(bencher: divan::Bencher) {
        let directory = tempdir().expect("create replay directory");
        let cache = Cache::open(directory.path(), DEFAULT_SIZE_LIMIT, &NAMESPACES).expect("open initial cache");
        populate(&cache);
        cache.shutdown().expect("close initial cache");

        bencher.bench_local(|| {
            let opened = Cache::open(directory.path(), DEFAULT_SIZE_LIMIT, &NAMESPACES).expect("open replay fixture");
            assert_eq!(opened.entry_count(), EXISTING_ENTRIES as usize);
            drop(CacheGuard(Some(opened)));
        });
    }
}
