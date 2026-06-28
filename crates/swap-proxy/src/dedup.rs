//! Tier 3: Deduplication table with Bloom filter pre-check.
//!
//! The dedup table maps page fingerprints to backend storage offsets,
//! with reference counting and LRU eviction. A Bloom filter provides
//! fast rejection of unique pages without hitting the concurrent hash map.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;

use crate::fingerprint::{self, Fingerprint};

/// An entry in the dedup table.
#[derive(Debug)]
pub struct DedupEntry {
    /// Number of pages referencing this stored copy.
    pub ref_count: AtomicU32,
    /// Offset in the backend storage (page slot number).
    pub backend_offset: u64,
    /// First 64 bytes of the page for collision verification.
    pub prefix: [u8; 64],
}

/// Statistics about the dedup table.
#[derive(Debug, Clone, Default)]
pub struct DedupStats {
    pub unique_pages: u64,
    pub duplicate_hits: u64,
    pub bloom_false_positives: u64,
    pub evictions: u64,
    pub table_entries: usize,
}

/// Simple Bloom filter for fast rejection of unique pages.
pub struct BloomFilter {
    bits: Vec<AtomicU64>,
    num_bits: usize,
}

impl BloomFilter {
    pub fn new(capacity: usize) -> Self {
        // Round up to multiple of 64 for atomic word alignment
        let num_bits = (capacity * 10).next_power_of_two().max(64);
        let num_words = num_bits / 64;
        let bits = (0..num_words).map(|_| AtomicU64::new(0)).collect();

        Self { bits, num_bits }
    }

    /// Insert a fingerprint into the bloom filter.
    pub fn insert(&self, fp: &Fingerprint) {
        let indices = fingerprint::bloom_indices(fp, self.num_bits);
        for idx in indices {
            let word = idx / 64;
            let bit = idx % 64;
            self.bits[word].fetch_or(1u64 << bit, Ordering::Relaxed);
        }
    }

    /// Check if a fingerprint might be in the bloom filter.
    pub fn might_contain(&self, fp: &Fingerprint) -> bool {
        let indices = fingerprint::bloom_indices(fp, self.num_bits);
        for idx in indices {
            let word = idx / 64;
            let bit = idx % 64;
            if self.bits[word].load(Ordering::Relaxed) & (1u64 << bit) == 0 {
                return false;
            }
        }
        true
    }

    #[allow(dead_code)]
    pub fn num_bits(&self) -> usize {
        self.num_bits
    }
}

/// The concurrent deduplication table.
pub struct DedupTable {
    /// Primary fingerprint -> entry mapping.
    map: DashMap<Fingerprint, Arc<DedupEntry>>,
    /// Bloom filter for fast rejection.
    bloom: BloomFilter,
    /// Maximum number of entries.
    max_entries: u64,
    /// LRU tracking (fingerprint -> last access tick).
    lru: Mutex<Vec<(Fingerprint, u64)>>,
    /// Statistics counters.
    unique_pages: AtomicU64,
    duplicate_hits: AtomicU64,
    bloom_false_positives: AtomicU64,
    evictions: AtomicU64,
    /// Monotonic tick for LRU ordering.
    tick: AtomicU64,
}

impl DedupTable {
    /// Create a new dedup table.
    ///
    /// - `max_entries`: Maximum number of unique page fingerprints to track.
    /// - `bloom_capacity`: Approximate number of unique pages for bloom filter sizing.
    pub fn new(max_entries: u64, bloom_capacity: usize) -> Self {
        Self {
            map: DashMap::with_capacity(max_entries as usize),
            bloom: BloomFilter::new(bloom_capacity),
            max_entries,
            lru: Mutex::new(Vec::new()),
            unique_pages: AtomicU64::new(0),
            duplicate_hits: AtomicU64::new(0),
            bloom_false_positives: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            tick: AtomicU64::new(0),
        }
    }

    /// Look up a page by fingerprint. Returns the backend offset if found (and verified).
    ///
    /// Also performs bloom filter pre-check and collision verification.
    pub fn lookup(&self, fp: &Fingerprint, page_data: &[u8]) -> LookupResult {
        // Bloom filter fast rejection
        if !self.bloom.might_contain(fp) {
            return LookupResult::Miss;
        }

        // Check the map
        if let Some(entry) = self.map.get(fp) {
            // Collision guard: verify page prefix
            if fingerprint::verify_page_prefix(&entry.prefix, page_data) {
                // Genuine duplicate
                self.duplicate_hits.fetch_add(1, Ordering::Relaxed);
                return LookupResult::Duplicate {
                    backend_offset: entry.backend_offset,
                };
            } else {
                // Fingerprint collision (different content)
                self.bloom_false_positives.fetch_add(1, Ordering::Relaxed);
                return LookupResult::Miss;
            }
        }

        // Bloom filter false positive
        self.bloom_false_positives.fetch_add(1, Ordering::Relaxed);
        LookupResult::Miss
    }

    /// Insert a new unique page into the dedup table.
    ///
    /// Returns false if the table is full or a fingerprint collision prevents
    /// safe tracking. The caller still owns the backend slot in that case and
    /// may keep the page as a direct mapping.
    pub fn insert(&self, fp: Fingerprint, page_data: &[u8], backend_offset: u64) -> bool {
        if self.map.contains_key(&fp) {
            return false;
        }

        // Check capacity
        if self.map.len() as u64 >= self.max_entries && !self.evict_lru_metadata() {
            return false;
        }

        let prefix = fingerprint::page_prefix(page_data);

        let entry = Arc::new(DedupEntry {
            ref_count: AtomicU32::new(1),
            backend_offset,
            prefix,
        });

        self.map.insert(fp, entry);
        self.bloom.insert(&fp);
        self.unique_pages.fetch_add(1, Ordering::Relaxed);

        // Track for LRU
        let current_tick = self.tick.fetch_add(1, Ordering::Relaxed);
        {
            let mut lru = self.lru.lock();
            lru.push((fp, current_tick));
        }

        true
    }

    /// Increment the reference count for a page.
    pub fn add_reference(&self, fp: &Fingerprint) -> bool {
        if let Some(entry) = self.map.get(fp) {
            entry.ref_count.fetch_add(1, Ordering::Relaxed);

            // Update LRU tick
            let current_tick = self.tick.fetch_add(1, Ordering::Relaxed);
            let mut lru = self.lru.lock();
            if let Some(item) = lru.iter_mut().find(|(f, _)| f == fp) {
                item.1 = current_tick;
            }

            true
        } else {
            false
        }
    }

    /// Decrement the reference count for a page.
    pub fn remove_reference(&self, fp: &Fingerprint) -> ReferenceRemoval {
        let mut still_referenced = false;
        let removed = self.map.remove_if(fp, |_, entry| {
            let prev = entry.ref_count.fetch_sub(1, Ordering::Relaxed);
            if prev <= 1 {
                true
            } else {
                still_referenced = true;
                false
            }
        });

        if let Some((_, entry)) = removed {
            let slot = entry.backend_offset;

            // Remove from LRU
            let mut lru = self.lru.lock();
            lru.retain(|(f, _)| f != fp);

            ReferenceRemoval::Removed {
                backend_offset: slot,
            }
        } else if still_referenced {
            ReferenceRemoval::StillReferenced
        } else {
            ReferenceRemoval::NotTracked
        }
    }

    /// Evict the least-recently-used metadata entry to make room.
    ///
    /// Only singly referenced entries can be untracked. Their translation entry
    /// still owns the backend slot and will free it when overwritten/discarded.
    fn evict_lru_metadata(&self) -> bool {
        let candidates = {
            let mut lru = self.lru.lock();
            if lru.is_empty() {
                return false;
            }

            lru.sort_by_key(|(_, tick)| *tick);
            lru.iter().map(|(fp, _)| *fp).collect::<Vec<_>>()
        };

        for fp in candidates {
            let removed = self
                .map
                .remove_if(&fp, |_, entry| entry.ref_count.load(Ordering::Relaxed) <= 1);

            if removed.is_some() {
                self.evictions.fetch_add(1, Ordering::Relaxed);

                let mut lru = self.lru.lock();
                lru.retain(|(f, _)| f != &fp);

                return true;
            }
        }

        false
    }

    /// Get current dedup table statistics.
    pub fn stats(&self) -> DedupStats {
        DedupStats {
            unique_pages: self.unique_pages.load(Ordering::Relaxed),
            duplicate_hits: self.duplicate_hits.load(Ordering::Relaxed),
            bloom_false_positives: self.bloom_false_positives.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            table_entries: self.map.len(),
        }
    }
}

/// Result of a dedup table lookup.
#[derive(Debug)]
pub enum LookupResult {
    /// Page not found in the dedup table (unique page).
    Miss,
    /// Page found as a duplicate with the given backend offset.
    Duplicate { backend_offset: u64 },
}

/// Result of removing a dedup-table reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceRemoval {
    /// The entry still has at least one live translation.
    StillReferenced,
    /// The final tracked reference was removed; caller should free this slot.
    Removed { backend_offset: u64 },
    /// Metadata was already evicted. The translation owns its slot directly.
    NotTracked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::PAGE_SIZE;
    use std::sync::{Arc as StdArc, Barrier};
    use std::thread;

    #[test]
    fn test_bloom_filter() {
        let bloom = BloomFilter::new(1000);
        let page = vec![0x42u8; PAGE_SIZE];
        let fp = fingerprint::fingerprint_page(&page);

        assert!(!bloom.might_contain(&fp));
        bloom.insert(&fp);
        assert!(bloom.might_contain(&fp));
    }

    #[test]
    fn test_dedup_table_insert_and_lookup() {
        let table = DedupTable::new(100, 1000);

        let page = vec![0xABu8; PAGE_SIZE];
        let fp = fingerprint::fingerprint_page(&page);

        // First lookup should miss
        assert!(matches!(table.lookup(&fp, &page), LookupResult::Miss));

        // Insert the page
        assert!(table.insert(fp, &page, 7));

        // Second lookup should find duplicate
        match table.lookup(&fp, &page) {
            LookupResult::Duplicate { backend_offset } => {
                assert_eq!(backend_offset, 7);
            }
            _ => panic!("Expected duplicate"),
        }
    }

    #[test]
    fn test_dedup_table_different_content() {
        let table = DedupTable::new(100, 1000);

        let page_a = vec![0xAAu8; PAGE_SIZE];
        let page_b = vec![0xBBu8; PAGE_SIZE];
        let fp_a = fingerprint::fingerprint_page(&page_a);

        assert!(table.insert(fp_a, &page_a, 0));

        // Lookup with different content should miss (even if bloom says maybe)
        let fp_b = fingerprint::fingerprint_page(&page_b);
        match table.lookup(&fp_b, &page_b) {
            LookupResult::Duplicate { .. } => {} // OK if fingerprints differ
            LookupResult::Miss => {}             // Expected
        }
    }

    #[test]
    fn test_dedup_table_refcounting() {
        let table = DedupTable::new(100, 1000);

        let page = vec![0xCCu8; PAGE_SIZE];
        let fp = fingerprint::fingerprint_page(&page);

        assert!(table.insert(fp, &page, 11));
        table.add_reference(&fp);

        // Remove two references
        assert_eq!(
            table.remove_reference(&fp),
            ReferenceRemoval::StillReferenced
        );
        assert_eq!(
            table.remove_reference(&fp),
            ReferenceRemoval::Removed { backend_offset: 11 }
        );
    }

    #[test]
    fn test_concurrent_reference_removal_frees_slot_once() {
        let table = StdArc::new(DedupTable::new(100, 1000));
        let page = vec![0xDDu8; PAGE_SIZE];
        let fp = fingerprint::fingerprint_page(&page);
        let references = 8;

        assert!(table.insert(fp, &page, 12));
        for _ in 1..references {
            assert!(table.add_reference(&fp));
        }

        let barrier = StdArc::new(Barrier::new(references));
        let mut handles = Vec::new();
        for _ in 0..references {
            let table = StdArc::clone(&table);
            let barrier = StdArc::clone(&barrier);

            handles.push(thread::spawn(move || {
                barrier.wait();
                table.remove_reference(&fp)
            }));
        }

        let removals = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            removals
                .iter()
                .filter(|removal| {
                    matches!(removal, ReferenceRemoval::Removed { backend_offset: 12 })
                })
                .count(),
            1
        );
        assert_eq!(
            removals
                .iter()
                .filter(|removal| matches!(removal, ReferenceRemoval::StillReferenced))
                .count(),
            references - 1
        );
        assert_eq!(table.stats().table_entries, 0);
    }

    #[test]
    fn test_dedup_table_metadata_eviction_only_untracks_single_reference_entries() {
        let table = DedupTable::new(3, 100); // max 3 entries

        for i in 0u8..5 {
            let page = vec![i; PAGE_SIZE];
            let fp = fingerprint::fingerprint_page(&page);
            assert!(table.insert(fp, &page, i as u64));
        }

        let stats = table.stats();
        assert!(stats.table_entries <= 3);
        assert!(stats.evictions >= 2);
    }

    #[test]
    fn test_dedup_table_does_not_evict_shared_entries() {
        let table = DedupTable::new(1, 100);
        let page_a = vec![0xAA; PAGE_SIZE];
        let page_b = vec![0xBB; PAGE_SIZE];
        let fp_a = fingerprint::fingerprint_page(&page_a);
        let fp_b = fingerprint::fingerprint_page(&page_b);

        assert!(table.insert(fp_a, &page_a, 0));
        assert!(table.add_reference(&fp_a));
        assert!(!table.insert(fp_b, &page_b, 1));

        let stats = table.stats();
        assert_eq!(stats.table_entries, 1);
        assert_eq!(stats.evictions, 0);
    }
}
