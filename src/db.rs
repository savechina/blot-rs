use std::fs::File;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use crate::common::meta::Meta;
use crate::errors::Result;
use crate::tx::{Tx, TxApi, TxStats};

// FreelistType enum (replace with actual variants)
enum FreelistType {
    Array,
    HashMap,
}
pub trait DbApi: Clone + Send + Sync
where
    Self: Sized,
{
    // Path returns the path to currently open database file.
    fn path(&self) -> String;

    // Begin starts a new transaction.
    // Multiple read-only transactions can be used concurrently but only one
    // write transaction can be used at a time. Starting multiple write transactions
    // will cause the calls to block and be serialized until the current write
    // transaction finishes.
    //
    // Transactions should not be dependent on one another. Opening a read
    // transaction and a write transaction in the same goroutine can cause the
    // writer to deadlock because the database periodically needs to re-mmap itself
    // as it grows and it cannot do that while a read transaction is open.
    //
    // If a long running read transaction (for example, a snapshot transaction) is
    // needed, you might want to set DB.InitialMmapSize to a large enough value
    // to avoid potential blocking of write transaction.
    //
    // IMPORTANT: You must close read-only transactions after you are finished or
    // else the database will not reclaim old pages.
    fn begin(&self) -> crate::Result<impl TxApi>;

    // Begin starts a new transaction.
    // Multiple read-only transactions can be used concurrently but only one
    // write transaction can be used at a time. Starting multiple write transactions
    // will cause the calls to block and be serialized until the current write
    // transaction finishes.
    //
    // Transactions should not be dependent on one another. Opening a read
    // transaction and a write transaction in the same goroutine can cause the
    // writer to deadlock because the database periodically needs to re-mmap itself
    // as it grows and it cannot do that while a read transaction is open.
    //
    // If a long running read transaction (for example, a snapshot transaction) is
    // needed, you might want to set DB.InitialMmapSize to a large enough value
    // to avoid potential blocking of write transaction.
    //
    // IMPORTANT: You must close read-only transactions after you are finished or
    // else the database will not reclaim old pages.
    fn begin_rw(&mut self) -> crate::Result<impl TxApi>;

    // View executes a function within the context of a managed read-only transaction.
    // Any error that is returned from the function is returned from the View() method.
    //
    // Attempting to manually rollback within the function will cause a panic.
    fn view<'tx, Handler>(&'tx self, handler: Handler) -> crate::Result<()>
    where
        Handler: FnMut(Tx<'tx>) -> crate::Result<()>;

    // Update executes a function within the context of a read-write managed transaction.
    // If no error is returned from the function then the transaction is committed.
    // If an error is returned then the entire transaction is rolled back.
    // Any error that is returned from the function or returned from the commit is
    // returned from the Update() method.
    //
    // Attempting to manually commit or rollback within the function will cause a panic.
    fn update<'tx, Handler>(&'tx mut self, handler: Handler) -> crate::Result<()>
    where
        Handler: FnMut(Tx<'tx>) -> crate::Result<()>;

    // Batch calls fn as part of a batch. It behaves similar to Update,
    // except:
    //
    // 1. concurrent Batch calls can be combined into a single Bolt
    // transaction.
    //
    // 2. the function passed to Batch may be called multiple times,
    // regardless of whether it returns error or not.
    //
    // This means that Batch function side effects must be idempotent and
    // take permanent effect only after a successful return is seen in
    // caller.
    //
    // The maximum batch size and delay can be adjusted with DB.MaxBatchSize
    // and DB.MaxBatchDelay, respectively.
    //
    // Batch is only useful when there are multiple goroutines calling it.
    fn batch<'tx, Handler>(&'tx mut self, handler: Handler) -> crate::Result<()>
    where
        Handler: FnMut(Tx<'tx>) -> crate::Result<()> + Send + Sync + Clone + 'static;

    // Close releases all database resources.
    // It will block waiting for any open transactions to finish
    // before closing the database and returning.
    fn close(self) -> crate::Result<()>;

    // Sync executes fdatasync() against the database file handle.
    //
    // This is not necessary under normal operation, however, if you use NoSync
    // then it allows you to force the database file to sync against the disk.

    fn sync(&mut self) -> crate::Result<()>;

    // Stats retrieves ongoing performance stats for the database.
    // This is only updated when a transaction closes.
    fn stats(&self) -> Arc<Stats>;

    // This is for internal access to the raw data bytes from the C cursor, use
    // carefully, or not at all.
    fn info(&self) -> Info;
}

pub(crate) struct RawDB<'tx> {
    stats: Arc<Mutex<Stats>>, // Thread-safe access to statistics

    // Flags with explicit defaults
    strict_mode: bool,
    no_sync: bool,
    no_freelist_sync: bool,
    freelist_type: FreelistType,
    no_grow_sync: bool,
    pre_load_freelist: bool,
    mmap_flags: i32,

    // Configuration options
    max_batch_size: isize,
    max_batch_delay: Duration,
    alloc_size: usize,
    mlock: bool,

    // logger: Option<Logger>, // Optional logger
    path: String,
    file: Option<Arc<Mutex<File>>>, // Thread-safe file handle
    dataref: Option<Vec<u8>>,       // Optional mmap'ed data (read-only)
    data: Option<Box<[u8]>>,        // Optional data pointer (writeable)
    datasz: usize,

    meta0: Option<Arc<Mutex<Meta>>>, // Thread-safe meta page 0
    meta1: Option<Arc<Mutex<Meta>>>, // Thread-safe meta page 1

    page_size: usize,

    opened: bool,
    rwtx: Option<Arc<Mutex<Tx<'tx>>>>, // Read-write transaction (writer)
    txs: Vec<Arc<Mutex<Tx<'tx>>>>,     // Read-only transactions

    freelist: Option<Arc<Mutex<Freelist>>>, // Thread-safe freelist access
    freelist_load: Mutex<bool>,             // Flag to track freelist loading

    page_pool: Mutex<Vec<Box<[u8]>>>, // Pool of allocated pages

    batch_mu: Mutex<Option<Batch>>, // Mutex for batch operations
    rwlock: Mutex<()>,              // Mutex for single writer access

    metalock: Mutex<()>,  // Mutex for meta page access
    mmaplock: RwLock<()>, // RWLock for mmap access during remapping

    statlock: RwLock<()>, // RWLock for stats access

    ops: Ops, // Operations struct for file access

    read_only: bool, // Read-only mode flag
}

struct Ops {
    write_at: fn(&[u8], i64) -> Result<usize>,
}

#[derive(Clone)]
pub struct DB<'tx>(pub(crate) Arc<RawDB<'tx>>);

impl<'tx> DB<'tx> {
    pub(crate) fn begin_tx(&self) -> crate::Result<Tx<'tx>> {
        let raw_db = self.0.clone();
        let tx = Tx::new();
        Ok(tx)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WeakDB<'tx>(Weak<RawDB<'tx>>);

impl<'tx> WeakDB<'tx> {
    pub(crate) fn new() -> WeakDB<'tx> {
        WeakDB(Weak::new())
    }

    pub(crate) fn upgrade(&self) -> Option<DB<'tx>> {
        self.0.upgrade().map(DB)
    }

    pub(crate) fn from(db: &DB<'tx>) -> WeakDB<'tx> {
        WeakDB(Arc::downgrade(&db.0))
    }
}

pub struct Options;

struct Freelist;

struct Batch;

// Stats represents statistics about the database.
#[derive(Default, Clone)]
struct Stats {
    // Put `TxStats` at the first field to ensure it's 64-bit aligned. Note
    // that the first word in an allocated struct can be relied upon to be
    // 64-bit aligned. Refer to https://pkg.go.dev/sync/atomic#pkg-note-BUG.
    // Also refer to discussion in https://github.com/etcd-io/bbolt/issues/577.
    tx_stats: TxStats, // global, ongoing stats.

    // Freelist stats
    free_page_n: i32,    // total number of free pages on the freelist
    pending_page_n: i32, // total number of pending pages on the freelist
    free_alloc: i32,     // total bytes allocated in free pages
    freelist_inuse: i32, // total bytes used by the freelist

    // Transaction stats
    tx_n: i32,      // total number of started read transactions
    open_tx_n: i32, // number of currently open read transactions
}

impl Stats {
    fn new() -> Self {
        Stats {
            tx_stats: TxStats::default(),
            free_page_n: 0,
            pending_page_n: 0,
            free_alloc: 0,
            freelist_inuse: 0,
            tx_n: 0,
            open_tx_n: 0,
        }
    }
    pub fn get_tx_stats(&self) -> TxStats {
        self.tx_stats
    }
    pub fn get_free_page_n(&self) -> i32 {
        self.free_page_n
    }
    pub fn get_pending_page_n(&self) -> i32 {
        self.pending_page_n
    }
    pub fn get_free_alloc(&self) -> i32 {
        self.free_alloc
    }

    // Sub calculates and returns the difference between two sets of database stats.
    // This is useful when obtaining stats at two different points and time and
    // you need the performance counters that occurred within that time span.
    fn sub(&self, other: Option<&Stats>) -> Stats {
        if other.is_none() {
            return self.clone(); // Return a copy of self if other is None
        }

        let other = other.unwrap();

        Stats {
            tx_stats: self.tx_stats.sub(&other.tx_stats),
            free_page_n: self.free_page_n,
            pending_page_n: self.pending_page_n,
            free_alloc: self.free_alloc,
            freelist_inuse: self.freelist_inuse,
            tx_n: self.tx_n - other.tx_n,
            open_tx_n: self.open_tx_n,
        }
    }
}
struct Info {
    data: NonNull<u8>, // 使用 NonNull<u8> 替换 *const u8
    page_size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db() {
        // let db = DB(Arc::new(RawDB {
        //     stats: Arc::new(Mutex::new(Stats {})),
        //     strict_mode: false,
        //     no_sync: false,
        //     no_freelist_sync: false,
        //     freelist_type: FreelistType::Array,
        //     no_grow_sync: false,
        //     pre_load_freelist: false,
        //     mmap_flags: 0,
        //     max_batch_size: 0,
        //     max_batch_delay: Duration::from_secs(0),
        //     alloc_size: 0,
        //     mlock: false,
        //     ..Default::default()
        // }));
        // let tx = db.begin_tx().unwrap();
        // assert_eq!(tx.writable(), false);
    }

    #[test]
    fn test_db_info() {
        let data = String::from("test db info");

        let len = data.len();

        let data_ptr: *mut u8 = data.as_ptr() as *mut u8; // 示例内存地址
        let non_null_ptr = NonNull::new(data_ptr);

        if let Some(ptr) = non_null_ptr {
            let info = Info {
                data: ptr,
                page_size: 4096,
            };

            // 安全地使用 NonNull 指针（仍然需要 unsafe 块）
            unsafe {
                let value: u8 = *info.data.as_ptr();
                println!("Data value: {:?}", value);
            }

            println!("Page size: {}", info.page_size);
        } else {
            println!("Data pointer is null");
        }
    }
}
