use super::{super::Config, watchers::Watchers, DatabaseEngine, Tree};
use crate::{utils, Result};
use std::{future::Future, pin::Pin, sync::Arc, collections::HashMap, sync::RwLock};

pub struct Engine {
    rocks: rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>,
    cache: rocksdb::Cache,
    old_cfs: Vec<String>,
}

pub struct RocksDbEngineTree<'a> {
    db: Arc<Engine>,
    name: &'a str,
    watchers: Watchers,
    write_lock: RwLock<()>
}

impl DatabaseEngine for Arc<Engine> {
    fn open(config: &Config) -> Result<Self> {
        let rocksdb_cache =
            rocksdb::Cache::new_lru_cache((config.db_cache_capacity_mb * 1024.0 * 1024.0) as usize)
                .unwrap();

        let mut block_based_options = rocksdb::BlockBasedOptions::default();
        block_based_options.set_block_cache(&rocksdb_cache);

        let mut db_opts = rocksdb::Options::default();
        db_opts.set_block_based_table_factory(&block_based_options);
        db_opts.create_if_missing(true);
        db_opts.increase_parallelism(num_cpus::get() as i32);
        db_opts.set_max_open_files(512);
        db_opts.set_compression_type(rocksdb::DBCompressionType::Zstd);
        db_opts.set_compaction_style(rocksdb::DBCompactionStyle::Level);
        db_opts.optimize_level_style_compaction((config.db_cache_capacity_mb * 1024.0 * 1024.0) as usize);

        let cfs = rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::list_cf(
            &db_opts,
            &config.database_path,
        )
        .unwrap_or_default();

        let db = rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::open_cf_descriptors(
            &db_opts,
            &config.database_path,
            cfs.iter().map(|name| {
                let mut options = rocksdb::Options::default();
                let prefix_extractor = rocksdb::SliceTransform::create_fixed_prefix(1);
                options.set_prefix_extractor(prefix_extractor);

                rocksdb::ColumnFamilyDescriptor::new(name, options)
            }),
        )?;

        Ok(Arc::new(Engine {
            rocks: db,
            cache: rocksdb_cache,
            old_cfs: cfs,
        }))
    }

    fn open_tree(&self, name: &'static str) -> Result<Arc<dyn Tree>> {
        if !self.old_cfs.contains(&name.to_owned()) {
            // Create if it didn't exist
            let mut options = rocksdb::Options::default();
            let prefix_extractor = rocksdb::SliceTransform::create_fixed_prefix(1);
            options.set_prefix_extractor(prefix_extractor);

            let _ = self.rocks.create_cf(name, &options);
        }

        Ok(Arc::new(RocksDbEngineTree {
            name,
            db: Arc::clone(self),
            watchers: Watchers::default(),
            write_lock: RwLock::new(()),
        }))
    }

    fn flush(&self) -> Result<()> {
        // TODO?
        Ok(())
    }

    fn memory_usage(&self) -> Result<String> {
        let stats =
            rocksdb::perf::get_memory_usage_stats(Some(&[&self.rocks]), Some(&[&self.cache]))?;
        Ok(format!(
            "Approximate memory usage of all the mem-tables: {:.3} MB\n\
             Approximate memory usage of un-flushed mem-tables: {:.3} MB\n\
             Approximate memory usage of all the table readers: {:.3} MB\n\
             Approximate memory usage by cache: {:.3} MB\n\
             self.cache.get_usage(): {:.3} MB\n\
             self.cache.get_pinned_usage(): {:.3} MB\n\
             ",
            stats.mem_table_total as f64 / 1024.0 / 1024.0,
            stats.mem_table_unflushed as f64 / 1024.0 / 1024.0,
            stats.mem_table_readers_total as f64 / 1024.0 / 1024.0,
            stats.cache_total as f64 / 1024.0 / 1024.0,
            self.cache.get_usage() as f64 / 1024.0 / 1024.0,
            self.cache.get_pinned_usage() as f64 / 1024.0 / 1024.0,
        ))
    }
}

impl RocksDbEngineTree<'_> {
    fn cf(&self) -> Arc<rocksdb::BoundColumnFamily<'_>> {
        self.db.rocks.cf_handle(self.name).unwrap()
    }
}

impl Tree for RocksDbEngineTree<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.rocks.get_cf(&self.cf(), key)?)
    }

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let lock = self.write_lock.read().unwrap();
        self.db.rocks.put_cf(&self.cf(), key, value)?;
        drop(lock);

        self.watchers.wake(key);

        Ok(())
    }

    fn insert_batch<'a>(&self, iter: &mut dyn Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Result<()> {
        for (key, value) in iter {
            self.db.rocks.put_cf(&self.cf(), key, value)?;
        }

        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        Ok(self.db.rocks.delete_cf(&self.cf(), key)?)
    }

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        Box::new(
            self.db
                .rocks
                .iterator_cf(&self.cf(), rocksdb::IteratorMode::Start)
                .map(|(k, v)| (Vec::from(k), Vec::from(v))),
        )
    }

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        Box::new(
            self.db
                .rocks
                .iterator_cf(
                    &self.cf(),
                    rocksdb::IteratorMode::From(
                        from,
                        if backwards {
                            rocksdb::Direction::Reverse
                        } else {
                            rocksdb::Direction::Forward
                        },
                    ),
                )
                .map(|(k, v)| (Vec::from(k), Vec::from(v))),
        )
    }

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>> {
        let lock = self.write_lock.write().unwrap();

        let old = self.db.rocks.get_cf(&self.cf(), &key)?;
        let new = utils::increment(old.as_deref()).unwrap();
        self.db.rocks.put_cf(&self.cf(), key, &new)?;

        drop(lock);
        Ok(new)
    }

    fn increment_batch<'a>(&self, iter: &mut dyn Iterator<Item = Vec<u8>>) -> Result<()> {
        let lock = self.write_lock.write().unwrap();

        for key in iter {
            let old = self.db.rocks.get_cf(&self.cf(), &key)?;
            let new = utils::increment(old.as_deref()).unwrap();
            self.db.rocks.put_cf(&self.cf(), key, new)?;
        }

        drop(lock);

        Ok(())
    }

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        Box::new(
            self.db
                .rocks
                .iterator_cf(
                    &self.cf(),
                    rocksdb::IteratorMode::From(&prefix, rocksdb::Direction::Forward),
                )
                .map(|(k, v)| (Vec::from(k), Vec::from(v)))
                .take_while(move |(k, _)| k.starts_with(&prefix)),
        )
    }

    fn watch_prefix<'a>(&'a self, prefix: &[u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.watchers.watch(prefix)
    }
}
