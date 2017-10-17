// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;
use std::fmt::{self, Display, Formatter};
use std::collections::BinaryHeap;
use std::cmp::Ordering;

use rocksdb::{SeekKey, DB};

use kvproto::metapb::RegionEpoch;
use kvproto::metapb::Region;

use raftstore::store::{keys, Msg};
use raftstore::store::engine::{IterOption, Iterable};
use raftstore::store::util;
use raftstore::Result;
use rocksdb::DBIterator;
use util::escape;
use util::transport::{RetryableSendCh, Sender};
use util::worker::Runnable;
use storage::{CfName, LARGE_CFS};

use super::metrics::*;

#[derive(PartialEq, Eq)]
struct KeyEntry {
    key: Option<Vec<u8>>,
    pos: usize,
    value_size: usize,
}

impl KeyEntry {
    fn new(key: Vec<u8>, pos: usize, value_size: usize) -> KeyEntry {
        KeyEntry {
            key: Some(key),
            pos: pos,
            value_size: value_size,
        }
    }

    fn take(&mut self) -> KeyEntry {
        KeyEntry::new(self.key.take().unwrap(), self.pos, self.value_size)
    }
}

impl PartialOrd for KeyEntry {
    fn partial_cmp(&self, rhs: &KeyEntry) -> Option<Ordering> {
        // BinaryHeap is max heap, so we have to reverse order to get a min heap.
        Some(
            self.key
                .as_ref()
                .unwrap()
                .cmp(rhs.key.as_ref().unwrap())
                .reverse(),
        )
    }
}

impl Ord for KeyEntry {
    fn cmp(&self, rhs: &KeyEntry) -> Ordering {
        self.partial_cmp(rhs).unwrap()
    }
}

struct MergedIterator<'a> {
    iters: Vec<DBIterator<&'a DB>>,
    heap: BinaryHeap<KeyEntry>,
}

impl<'a> MergedIterator<'a> {
    fn new(
        db: &'a DB,
        cfs: &[CfName],
        start_key: &[u8],
        end_key: &[u8],
        fill_cache: bool,
    ) -> Result<MergedIterator<'a>> {
        let mut iters = Vec::with_capacity(cfs.len());
        let mut heap = BinaryHeap::with_capacity(cfs.len());
        for (pos, cf) in cfs.into_iter().enumerate() {
            let iter_opt = IterOption::new(Some(end_key.to_vec()), fill_cache);
            let mut iter = db.new_iterator_cf(cf, iter_opt)?;
            if iter.seek(start_key.into()) {
                heap.push(KeyEntry::new(iter.key().to_vec(), pos, iter.value().len()));
            }
            iters.push(iter);
        }
        Ok(MergedIterator {
            iters: iters,
            heap: heap,
        })
    }

    fn next(&mut self) -> Option<KeyEntry> {
        let pos = match self.heap.peek() {
            None => return None,
            Some(e) => e.pos,
        };
        let iter = &mut self.iters[pos];
        if iter.next() {
            // TODO: avoid copy key.
            let e = KeyEntry::new(iter.key().to_vec(), pos, iter.value().len());
            let mut front = self.heap.peek_mut().unwrap();
            let res = front.take();
            *front = e;
            Some(res)
        } else {
            self.heap.pop()
        }
    }
}

#[allow(collapsible_if)]
fn bound_keys(
    db: &DB,
    cfs: &[CfName],
    start_key: &[u8],
    end_key: &[u8],
) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut first_key = None;
    let mut last_key = None;

    for cf in cfs {
        let iter_opt = IterOption::new(Some(end_key.to_vec()), false);
        let mut iter = db.new_iterator_cf(cf, iter_opt)?;

        // the first key
        let key = if iter.seek(start_key.into()) {
            Some(iter.key().to_vec())
        } else {
            if iter.next() {
                Some(iter.key().to_vec())
            } else {
                // No data in this CF.
                None
            }
        };
        if first_key.is_some() {
            if key.is_some() {
                if key.as_ref().unwrap() < first_key.as_ref().unwrap() {
                    first_key = key;
                }
            }
        } else {
            first_key = key;
        }

        // the last key
        let key = if iter.seek(SeekKey::End) {
            Some(iter.key().to_vec())
        } else {
            // No dat in this CF.
            None
        };
        if last_key.is_some() {
            if key.is_some() {
                if key.as_ref().unwrap() > last_key.as_ref().unwrap() {
                    last_key = key;
                }
            }
        } else {
            last_key = key;
        }
    }

    match (first_key, last_key) {
        (Some(fk), Some(lk)) => Ok(Some((fk, lk))),
        (None, None) => Ok(None),
        (first_key, last_key) => Err(box_err!(
            "invalid bound, first key: {:?}, last key: {:?}",
            first_key,
            last_key
        )),
    }
}

/// An interface for writing split checker extensions.
pub trait Checker: Send {
    // The Checker's name.
    fn name(&self) -> &str;
    /// Do some quick checks, true for skipping `find_split_key`.
    fn prev_check(&self, region: &Region, actual_keys: &Option<(Vec<u8>, Vec<u8>)>) -> bool;
    /// Feed keys and value sizes in order to find the split key.
    fn find_split_key(&mut self, key: &[u8], value_size: u64) -> Option<Vec<u8>>;
    /// Called at the end of check, for cleaning up.
    fn finish(&mut self);
}

struct SizeChecker<C> {
    engine: Arc<DB>,
    ch: RetryableSendCh<Msg, C>,
    region_max_size: u64,
    split_size: u64,

    split_key: Option<Vec<u8>>,
    current_size: u64,
}

impl<C: Sender<Msg> + Send> SizeChecker<C> {
    fn check_size(&self, region: &Region) -> Option<u64> {
        let region_id = region.get_id();
        let region_size = match util::get_region_approximate_size(&self.engine, region) {
            Ok(size) => size,
            Err(e) => {
                error!(
                    "[region {}] failed to get approximate size: {}",
                    region_id,
                    e
                );
                return None;
            }
        };

        let res = Msg::ApproximateRegionSize {
            region_id: region_id,
            region_size: region_size,
        };
        if let Err(e) = self.ch.try_send(res) {
            error!(
                "[region {}] failed to send approximate region size: {}",
                region_id,
                e
            );
        }

        REGION_SIZE_HISTOGRAM.observe(region_size as f64);
        Some(region_size)
    }
}

impl<C: Sender<Msg> + Send> Checker for SizeChecker<C> {
    fn name(&self) -> &str {
        "SizeChecker"
    }

    fn prev_check(&self, region: &Region, _: &Option<(Vec<u8>, Vec<u8>)>) -> bool {
        if let Some(region_size) = self.check_size(region) {
            if region_size < self.region_max_size {
                true
            } else {
                info!(
                    "[region {}] approximate size {} >= {}, need to do split check",
                    region.get_id(),
                    region_size,
                    self.region_max_size
                );
                false
            }
        } else {
            false
        }
    }

    fn find_split_key(&mut self, key: &[u8], value_size: u64) -> Option<Vec<u8>> {
        self.current_size += key.len() as u64 + value_size;
        if self.split_key.is_none() && self.current_size > self.split_size {
            self.split_key = Some(key.to_vec());
        }
        if self.split_key.is_some() && self.current_size >= self.region_max_size {
            return self.split_key.take();
        }
        None
    }

    fn finish(&mut self) {
        self.split_key = None;
        self.current_size = 0;
    }
}

/// Split checking task.
pub struct Task {
    region: Region,
}

impl Task {
    pub fn new(region: &Region) -> Task {
        Task {
            region: region.clone(),
        }
    }
}

impl Display for Task {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "Split Check Task for {}", self.region.get_id())
    }
}

pub struct Runner<C> {
    ch: RetryableSendCh<Msg, C>,
    engine: Arc<DB>,
    size_checker: SizeChecker<C>,
    priority_checker: Option<Box<Checker + 'static>>,
}

impl<C: Sender<Msg> + Send> Runner<C> {
    pub fn new(
        engine: Arc<DB>,
        ch: RetryableSendCh<Msg, C>,
        region_max_size: u64,
        split_size: u64,
    ) -> Runner<C> {
        Runner {
            engine: engine.clone(),
            ch: ch.clone(),
            size_checker: SizeChecker {
                engine: engine,
                ch: ch,
                region_max_size: region_max_size,
                split_size: split_size,
                split_key: None,
                current_size: 0,
            },
            priority_checker: None,
        }
    }

    pub fn set_priority_checker(&mut self, checker: Option<Box<Checker>>) {
        self.priority_checker = checker;
    }

    fn check_split(&mut self, region: &Region) {
        let region_id = region.get_id();
        let start_key = keys::enc_start_key(region);
        let end_key = keys::enc_end_key(region);
        let bks = match bound_keys(self.engine.as_ref(), LARGE_CFS, &start_key, &end_key) {
            Ok(bks) => bks,
            Err(e) => {
                error!("[region {}] failed to get region bound: {}", region_id, e);
                return;
            }
        };
        let skip_size_checker = self.size_checker.prev_check(region, &bks);
        let skip_priority_checker = self.priority_checker
            .as_ref()
            .map_or(true, |checker| checker.prev_check(region, &bks));
        if skip_priority_checker && skip_size_checker {
            return;
        }

        debug!(
            "[region {}] executing task {} {}",
            region_id,
            escape(&start_key),
            escape(&end_key)
        );
        CHECK_SPILT_COUNTER_VEC.with_label_values(&["all"]).inc();

        let mut size_split_key = None;
        let mut size_checker = &mut self.size_checker;
        let mut priority_split_key = None;
        let mut priority_checker = &mut self.priority_checker;

        let timer = CHECK_SPILT_HISTOGRAM.start_coarse_timer();
        let res = MergedIterator::new(self.engine.as_ref(), LARGE_CFS, &start_key, &end_key, false)
            .map(|mut iter| while let Some(e) = iter.next() {
                if !skip_priority_checker {
                    if let Some(key) = priority_checker.as_mut().map_or(None, |checker| {
                        checker.find_split_key(e.key.as_ref().unwrap(), e.value_size as u64)
                    }) {
                        info!(
                            "[region {}] priority split checker {} requires splitting at {:?}",
                            region_id,
                            priority_checker.as_ref().unwrap().name(),
                            key
                        );
                        priority_split_key = Some(key);
                        break;
                    }
                }
                if !skip_size_checker {
                    if let Some(key) =
                        size_checker.find_split_key(e.key.as_ref().unwrap(), e.value_size as u64)
                    {
                        info!(
                            "[region {}] priority split checker {} requires splitting at {:?}",
                            region_id,
                            size_checker.name(),
                            key
                        );
                        size_split_key = Some(key);
                        break;
                    }
                }
            });
        size_checker.finish();
        priority_checker.as_mut().map(|c| c.finish());
        timer.observe_duration();

        if let Err(e) = res {
            error!("[region {}] failed to scan split key: {}", region_id, e);
            return;
        }

        let split_key = match (priority_split_key, size_split_key) {
            (Some(key), _) | (None, Some(key)) => key,
            (None, None) => {
                CHECK_SPILT_COUNTER_VEC.with_label_values(&["ignore"]).inc();
                return;
            }
        };

        let region_epoch = region.get_region_epoch().clone();
        let res = self.ch
            .try_send(new_split_region(region_id, region_epoch, split_key));
        if let Err(e) = res {
            warn!("[region {}] failed to send check result: {}", region_id, e);
        }

        CHECK_SPILT_COUNTER_VEC
            .with_label_values(&["success"])
            .inc();
    }
}

impl<C: Sender<Msg> + Send> Runnable<Task> for Runner<C> {
    fn run(&mut self, task: Task) {
        self.check_split(&task.region);
    }
}

fn new_split_region(region_id: u64, epoch: RegionEpoch, split_key: Vec<u8>) -> Msg {
    let key = keys::origin_key(split_key.as_slice()).to_vec();
    Msg::SplitRegion {
        region_id: region_id,
        region_epoch: epoch,
        split_key: key,
        callback: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::Arc;

    use tempdir::TempDir;
    use rocksdb::Writable;
    use kvproto::metapb::Peer;
    use rocksdb::{ColumnFamilyOptions, DBOptions};

    use storage::ALL_CFS;
    use storage::types::Key;
    use util::rocksdb::{new_engine, new_engine_opt, CFOptions};
    use util::properties::SizePropertiesCollectorFactory;
    use coprocessor::codec::table;
    use raftstore::coprocessor::SplitTableChecker;
    use super::*;

    #[test]
    fn test_split_check() {
        let path = TempDir::new("test-raftstore").unwrap();
        let path_str = path.path().to_str().unwrap();
        let db_opts = DBOptions::new();
        let mut cf_opts = ColumnFamilyOptions::new();
        let f = Box::new(SizePropertiesCollectorFactory::default());
        cf_opts.add_table_properties_collector_factory("tikv.size-collector", f);
        let cfs_opts = ALL_CFS
            .iter()
            .map(|cf| CFOptions::new(cf, cf_opts.clone()))
            .collect();
        let engine = Arc::new(new_engine_opt(path_str, db_opts, cfs_opts).unwrap());

        let mut region = Region::new();
        region.set_id(1);
        region.set_start_key(vec![]);
        region.set_end_key(vec![]);
        region.mut_peers().push(Peer::new());
        region.mut_region_epoch().set_version(2);
        region.mut_region_epoch().set_conf_ver(5);

        let (tx, rx) = mpsc::sync_channel(100);
        let ch = RetryableSendCh::new(tx, "test-split");
        let mut runnable = Runner::new(engine.clone(), ch, 100, 60);

        // so split key will be z0006
        for i in 0..7 {
            let s = keys::data_key(format!("{:04}", i).as_bytes());
            engine.put(&s, &s).unwrap();
        }

        runnable.run(Task::new(&region));
        // size has not reached the max_size 100 yet.
        match rx.try_recv() {
            Ok(Msg::ApproximateRegionSize { region_id, .. }) => {
                assert_eq!(region_id, region.get_id());
            }
            others => panic!("expect recv empty, but got {:?}", others),
        }

        for i in 7..11 {
            let s = keys::data_key(format!("{:04}", i).as_bytes());
            engine.put(&s, &s).unwrap();
        }

        // Approximate size of memtable is inaccurate for small data,
        // we flush it to SST so we can use the size properties instead.
        engine.flush(true).unwrap();

        runnable.run(Task::new(&region));
        match rx.try_recv() {
            Ok(Msg::ApproximateRegionSize { region_id, .. }) => {
                assert_eq!(region_id, region.get_id());
            }
            others => panic!("expect approximate region size, but got {:?}", others),
        }
        match rx.try_recv() {
            Ok(Msg::SplitRegion {
                region_id,
                region_epoch,
                split_key,
                ..
            }) => {
                assert_eq!(region_id, region.get_id());
                assert_eq!(&region_epoch, region.get_region_epoch());
                assert_eq!(split_key, b"0006");
            }
            others => panic!("expect split check result, but got {:?}", others),
        }

        // So split key will be z0003
        for i in 0..6 {
            let s = keys::data_key(format!("{:04}", i).as_bytes());
            for cf in ALL_CFS {
                let handle = engine.cf_handle(cf).unwrap();
                engine.put_cf(handle, &s, &s).unwrap();
            }
        }
        for cf in ALL_CFS {
            let handle = engine.cf_handle(cf).unwrap();
            engine.flush_cf(handle, true).unwrap();
        }

        runnable.run(Task::new(&region));
        match rx.try_recv() {
            Ok(Msg::ApproximateRegionSize { region_id, .. }) => {
                assert_eq!(region_id, region.get_id());
            }
            others => panic!("expect approximate region size, but got {:?}", others),
        }
        match rx.try_recv() {
            Ok(Msg::SplitRegion {
                region_id,
                region_epoch,
                split_key,
                ..
            }) => {
                assert_eq!(region_id, region.get_id());
                assert_eq!(&region_epoch, region.get_region_epoch());
                assert_eq!(split_key, b"0003");
            }
            others => panic!("expect split check result, but got {:?}", others),
        }

        drop(rx);
        // It should be safe even the result can't be sent back.
        runnable.run(Task::new(&region));
    }

    #[test]
    fn test_split_table() {
        let path = TempDir::new("test-raftstore").unwrap();
        let engine = Arc::new(new_engine(path.path().to_str().unwrap(), ALL_CFS).unwrap());

        let mut region = Region::new();
        region.set_id(1);
        region.mut_peers().push(Peer::new());
        region.mut_region_epoch().set_version(2);
        region.mut_region_epoch().set_conf_ver(5);

        let (table_tx, table_rx) = mpsc::sync_channel(100);
        let table_ch = RetryableSendCh::new(table_tx, "test-split-table");
        let mut table_runnable = Runner::new(engine.clone(), table_ch, 200, 120);
        table_runnable.set_priority_checker(Some(Box::new(SplitTableChecker::default())));

        let check = |msg: Msg, key: Vec<u8>| match msg {
            Msg::SplitRegion { split_key, .. } => {
                assert_eq!(&split_key, Key::from_raw(&key).encoded())
            }
            others => panic!("expect split check result, but got {:?}", others),
        };

        // arbitrary padding.
        let padding = b"_r00000005";

        // Put some data
        // t1_xx, t3_xx, t5_xx
        for i in 1..6 {
            if i % 2 == 0 {
                // leave some space.
                continue;
            }

            let mut key = table::gen_table_prefix(i);
            key.extend_from_slice(padding);
            let s = keys::data_key(Key::from_raw(&key).encoded());
            engine.put(&s, &s).unwrap();
        }
        engine.flush(true).unwrap();

        // ["", "") => t3
        region.set_start_key(vec![]);
        region.set_end_key(vec![]);
        table_runnable.run(Task::new(&region));
        check(table_rx.try_recv().unwrap(), table::gen_table_prefix(3));

        // ["t1", "") => t3
        region.set_start_key(Key::from_raw(&table::gen_table_prefix(1)).encoded().clone());
        region.set_end_key(vec![]);
        table_runnable.run(Task::new(&region));
        check(table_rx.try_recv().unwrap(), table::gen_table_prefix(3));

        // ["t1", "t5") => t3
        region.set_start_key(Key::from_raw(&table::gen_table_prefix(1)).encoded().clone());
        region.set_end_key(Key::from_raw(&table::gen_table_prefix(5)).encoded().clone());
        table_runnable.run(Task::new(&region));
        check(table_rx.try_recv().unwrap(), table::gen_table_prefix(3));

        // Put some data to table 3.
        for i in 0..5 {
            // Each kv entry takes about 56 bytes.
            let mut key = table::gen_table_prefix(3);
            key.extend_from_slice(format!("_r0000000{}", i).as_bytes());
            let s = keys::data_key(Key::from_raw(&key).encoded());
            engine.put(&s, &s).unwrap();
        }
        // Since region_max_size = 200, split_size = 120
        // for ["t3", ""), the split key should be in table 3.
        region.set_start_key(Key::from_raw(&table::gen_table_prefix(3)).encoded().clone());
        region.set_end_key(vec![]);
        table_runnable.run(Task::new(&region));
        match table_rx.try_recv() {
            Ok(Msg::SplitRegion { split_key, .. }) => {
                let key = Key::from_encoded(split_key).raw().unwrap();
                assert_eq!(table::decode_table_id(&key).unwrap(), 3);
            }
            others => panic!("expect split check result, but got {:?}", others),
        }
    }
}
