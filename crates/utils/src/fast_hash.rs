use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasherDefault, Hasher},
};

const K: u64 = 0x517cc1b727220a95;

pub type FastHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;
pub type FastHashSet<K> = HashSet<K, BuildHasherDefault<FastHasher>>;

#[derive(Default)]
pub struct FastHasher {
    hash: u64,
}

impl FastHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(K);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let (chunk, rest) = bytes.split_at(8);
            self.add(u64::from_ne_bytes(chunk.try_into().expect("8-byte chunk")));
            bytes = rest;
        }
        if bytes.len() >= 4 {
            let (chunk, rest) = bytes.split_at(4);
            self.add(u64::from(u32::from_ne_bytes(chunk.try_into().expect("4-byte chunk"))));
            bytes = rest;
        }
        if bytes.len() >= 2 {
            let (chunk, rest) = bytes.split_at(2);
            self.add(u64::from(u16::from_ne_bytes(chunk.try_into().expect("2-byte chunk"))));
            bytes = rest;
        }
        if let Some(&byte) = bytes.first() {
            self.add(u64::from(byte));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_and_set_roundtrip() {
        let mut map = FastHashMap::default();
        map.insert(1u64, "a");
        map.insert(2u64, "b");
        assert_eq!(map.get(&1), Some(&"a"));
        assert_eq!(map.remove(&2), Some("b"));
        assert_eq!(map.len(), 1);

        let mut set = FastHashSet::default();
        assert!(set.insert(42u32));
        assert!(!set.insert(42u32));
        assert!(set.contains(&42));
    }
}
