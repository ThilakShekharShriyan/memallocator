//! A 4 KiB page cache in front of a write-ahead log.
//!
//! A write is durable when its record is in the log and `sync_data` has
//! returned. The page file is a checkpoint of bytes that were already logged.
//! Replay writes each record at an absolute page offset, so doing it twice
//! is the same as doing it once, and a torn tail record is ignored.
//!
//! Group commit pushes records through the MPMC queue. One committer thread
//! syncs a contiguous LSN prefix once. It does not publish LSN 3 while LSN 2
//! is still only in memory.

use crate::queue::MpmcQueue;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

pub const PAGE: usize = 4096;
/// Records per `sync_data` when the queue is kept full. The bench's knob.
pub const WAL_BATCH: usize = 32;
const WAL_MAGIC: &[u8; 4] = b"WAL1";

struct Frame {
    page: u32,
    bytes: Box<[u8; PAGE]>,
    dirty: bool,
    max_lsn: u64,
    referenced: bool,
}

struct PageCache {
    frames: Vec<Option<Frame>>,
    index: std::collections::HashMap<u32, usize>,
    hand: usize,
    file: File,
}

struct Wal {
    file: File,
    next_lsn: u64,
    flushed_lsn: u64,
    len: u64,
}

struct State {
    cache: PageCache,
    wal: Wal,
}

/// Volatile page cache plus a log in `dir/wal` and `dir/pages`.
pub struct Store {
    dir: PathBuf,
    state: Mutex<State>,
}

#[derive(Clone, Copy)]
struct Job {
    lsn: u64,
    page: u32,
    offset: u16,
    len: u8,
    data: [u8; 32],
}

struct Shared {
    store: Arc<Store>,
    queue: MpmcQueue<Job>,
    next_lsn: AtomicU64,
    flushed: AtomicU64,
    stop: AtomicBool,
    failed: AtomicBool,
    syncs: AtomicU64,
    batch: usize,
}

/// Many writers, one thread that syncs the log.
pub struct GroupCommit {
    shared: Arc<Shared>,
    committer: Option<JoinHandle<()>>,
}

/// Cloned onto writer threads. Shutdown stays on `GroupCommit`.
#[derive(Clone)]
pub struct CommitHandle {
    shared: Arc<Shared>,
}

impl Store {
    pub fn create(dir: &Path, frames: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let wal_path = dir.join("wal");
        let page_path = dir.join("pages");
        let mut wal_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&wal_path)?;
        wal_file.write_all(WAL_MAGIC)?;
        wal_file.write_all(&(PAGE as u32).to_le_bytes())?;
        wal_file.sync_data()?;
        let page_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&page_path)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            state: Mutex::new(State {
                cache: PageCache::new(page_file, frames)?,
                wal: Wal {
                    file: wal_file,
                    next_lsn: 1,
                    flushed_lsn: 0,
                    len: 8,
                },
            }),
        })
    }

    pub fn open(dir: &Path, frames: usize) -> io::Result<Self> {
        let wal_path = dir.join("wal");
        let page_path = dir.join("pages");
        let mut wal_file = OpenOptions::new().read(true).write(true).open(&wal_path)?;
        let page_file = OpenOptions::new().read(true).write(true).open(&page_path)?;
        let mut cache = PageCache::new(page_file, frames)?;
        let wal = Wal::recover(&mut wal_file, &mut cache)?;
        let mut state = State { cache, wal };
        state.checkpoint()?;
        Ok(Self {
            dir: dir.to_path_buf(),
            state: Mutex::new(state),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn next_lsn(&self) -> u64 {
        self.lock().wal.next_lsn
    }

    /// Append and apply. Durability waits for [`Self::sync`].
    pub fn write_unsynced(&self, page: u32, offset: u16, data: &[u8]) -> io::Result<u64> {
        check_range(offset, data.len())?;
        let mut state = self.lock();
        let lsn = state.wal.next_lsn;
        state.apply(lsn, page, offset, data)?;
        Ok(lsn)
    }

    pub fn sync(&self) -> io::Result<()> {
        self.lock().wal.sync()
    }

    pub fn read(&self, page: u32, offset: u16, len: usize) -> io::Result<Vec<u8>> {
        check_range(offset, len)?;
        let mut state = self.lock();
        let slot = state.resident(page)?;
        let frame = state.cache.frames[slot].as_ref().expect("resident frame");
        let start = usize::from(offset);
        Ok(frame.bytes[start..start + len].to_vec())
    }

    fn commit_prefix(&self, jobs: &[Job]) -> io::Result<()> {
        let mut state = self.lock();
        for job in jobs {
            state.apply(
                job.lsn,
                job.page,
                job.offset,
                &job.data[..usize::from(job.len)],
            )?;
        }
        state.wal.sync()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

impl State {
    fn apply(&mut self, lsn: u64, page: u32, offset: u16, data: &[u8]) -> io::Result<()> {
        self.wal.append(lsn, page, offset, data)?;
        let slot = self.resident(page)?;
        let frame = self.cache.frames[slot].as_mut().expect("resident frame");
        let start = usize::from(offset);
        frame.bytes[start..start + data.len()].copy_from_slice(data);
        frame.dirty = true;
        frame.max_lsn = frame.max_lsn.max(lsn);
        frame.referenced = true;
        Ok(())
    }

    fn resident(&mut self, page: u32) -> io::Result<usize> {
        if let Some(&slot) = self.cache.index.get(&page) {
            if let Some(frame) = self.cache.frames[slot].as_mut() {
                frame.referenced = true;
            }
            return Ok(slot);
        }
        let slot = if let Some(slot) = self.cache.frames.iter().position(|frame| frame.is_none()) {
            slot
        } else {
            self.victim()?
        };
        let (dirty_lsn, old_page) = match self.cache.frames[slot].as_ref() {
            Some(frame) => (frame.dirty.then_some(frame.max_lsn), Some(frame.page)),
            None => (None, None),
        };
        if let Some(max_lsn) = dirty_lsn {
            if max_lsn > self.wal.flushed_lsn {
                self.wal.sync()?;
            }
        }
        if let Some(old) = old_page {
            self.cache.writeback(slot)?;
            self.cache.index.remove(&old);
        }
        let bytes = read_page(&mut self.cache.file, page)?;
        self.cache.frames[slot] = Some(Frame {
            page,
            bytes: Box::new(bytes),
            dirty: false,
            max_lsn: 0,
            referenced: true,
        });
        self.cache.index.insert(page, slot);
        Ok(slot)
    }

    fn victim(&mut self) -> io::Result<usize> {
        let n = self.cache.frames.len();
        for _ in 0..(n * 2) {
            let slot = self.cache.hand;
            self.cache.hand = (self.cache.hand + 1) % n;
            let referenced = self.cache.frames[slot]
                .as_ref()
                .map(|frame| frame.referenced)
                .unwrap_or(false);
            if referenced {
                if let Some(frame) = self.cache.frames[slot].as_mut() {
                    frame.referenced = false;
                }
                continue;
            }
            return Ok(slot);
        }
        Ok(0)
    }

    fn checkpoint(&mut self) -> io::Result<()> {
        for slot in 0..self.cache.frames.len() {
            let dirty = self.cache.frames[slot]
                .as_ref()
                .map(|frame| frame.dirty)
                .unwrap_or(false);
            if dirty {
                self.cache.writeback(slot)?;
            }
        }
        self.cache.file.sync_data()
    }
}

impl PageCache {
    fn new(file: File, frames: usize) -> io::Result<Self> {
        let frames = frames.max(1);
        Ok(Self {
            frames: (0..frames).map(|_| None).collect(),
            index: std::collections::HashMap::new(),
            hand: 0,
            file,
        })
    }

    fn writeback(&mut self, slot: usize) -> io::Result<()> {
        let Some(frame) = self.frames[slot].as_mut() else {
            return Ok(());
        };
        if !frame.dirty {
            return Ok(());
        }
        self.file
            .seek(SeekFrom::Start(u64::from(frame.page) * PAGE as u64))?;
        self.file.write_all(frame.bytes.as_ref())?;
        frame.dirty = false;
        Ok(())
    }
}

impl Wal {
    fn recover(file: &mut File, cache: &mut PageCache) -> io::Result<Self> {
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != WAL_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad wal magic"));
        }
        let mut page_size = [0u8; 4];
        file.read_exact(&mut page_size)?;
        if u32::from_le_bytes(page_size) != PAGE as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "wal page size does not match",
            ));
        }
        let mut len = 8u64;
        let mut next_lsn = 1u64;
        loop {
            let mut header = [0u8; 16];
            match read_full(file, &mut header)? {
                ReadFull::Eof => break,
                ReadFull::Short => {
                    file.set_len(len)?;
                    break;
                }
                ReadFull::Full => {}
            }
            let lsn = u64::from_le_bytes(header[0..8].try_into().unwrap());
            let page = u32::from_le_bytes(header[8..12].try_into().unwrap());
            let offset = u16::from_le_bytes(header[12..14].try_into().unwrap());
            let rec_len = u16::from_le_bytes(header[14..16].try_into().unwrap()) as usize;
            if lsn != next_lsn || rec_len > PAGE || usize::from(offset) + rec_len > PAGE {
                file.set_len(len)?;
                break;
            }
            let mut data = vec![0u8; rec_len];
            match read_full(file, &mut data)? {
                ReadFull::Full => {}
                ReadFull::Eof | ReadFull::Short => {
                    file.set_len(len)?;
                    break;
                }
            }
            apply_replay(cache, page, offset, &data)?;
            len += 16 + rec_len as u64;
            next_lsn = lsn + 1;
        }
        file.seek(SeekFrom::Start(len))?;
        Ok(Self {
            file: file.try_clone()?,
            next_lsn,
            flushed_lsn: next_lsn.saturating_sub(1),
            len,
        })
    }

    fn append(&mut self, lsn: u64, page: u32, offset: u16, data: &[u8]) -> io::Result<()> {
        if lsn != self.next_lsn {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "wal lsn is not the next contiguous record",
            ));
        }
        if data.len() > PAGE || usize::from(offset) + data.len() > PAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "record past page",
            ));
        }
        self.file.seek(SeekFrom::Start(self.len))?;
        self.file.write_all(&lsn.to_le_bytes())?;
        self.file.write_all(&page.to_le_bytes())?;
        self.file.write_all(&offset.to_le_bytes())?;
        self.file.write_all(&(data.len() as u16).to_le_bytes())?;
        self.file.write_all(data)?;
        self.len += 16 + data.len() as u64;
        self.next_lsn += 1;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()?;
        self.flushed_lsn = self.next_lsn.saturating_sub(1);
        Ok(())
    }
}

fn apply_replay(cache: &mut PageCache, page: u32, offset: u16, data: &[u8]) -> io::Result<()> {
    let slot = if let Some(&slot) = cache.index.get(&page) {
        slot
    } else if let Some(slot) = cache.frames.iter().position(|frame| frame.is_none()) {
        let bytes = read_page(&mut cache.file, page)?;
        cache.frames[slot] = Some(Frame {
            page,
            bytes: Box::new(bytes),
            dirty: false,
            max_lsn: 0,
            referenced: true,
        });
        cache.index.insert(page, slot);
        slot
    } else {
        // Recovery may see more pages than frames. Checkpoint the clock victim
        // so the bytes already replayed are in the page file, then reuse it.
        let slot = cache.hand;
        cache.hand = (cache.hand + 1) % cache.frames.len();
        let old = cache.frames[slot].as_ref().map(|frame| frame.page);
        if let Some(old) = old {
            cache.writeback(slot)?;
            cache.index.remove(&old);
        }
        let bytes = read_page(&mut cache.file, page)?;
        cache.frames[slot] = Some(Frame {
            page,
            bytes: Box::new(bytes),
            dirty: false,
            max_lsn: 0,
            referenced: true,
        });
        cache.index.insert(page, slot);
        slot
    };
    let frame = cache.frames[slot].as_mut().expect("replay frame");
    let start = usize::from(offset);
    frame.bytes[start..start + data.len()].copy_from_slice(data);
    frame.dirty = true;
    frame.referenced = true;
    Ok(())
}

enum ReadFull {
    Full,
    Short,
    Eof,
}

fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<ReadFull> {
    if buf.is_empty() {
        return Ok(ReadFull::Full);
    }
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(ReadFull::Eof),
            0 => return Ok(ReadFull::Short),
            n => filled += n,
        }
    }
    Ok(ReadFull::Full)
}

fn read_page(file: &mut File, page: u32) -> io::Result<[u8; PAGE]> {
    let mut bytes = [0u8; PAGE];
    file.seek(SeekFrom::Start(u64::from(page) * PAGE as u64))?;
    let mut filled = 0;
    while filled < PAGE {
        match file.read(&mut bytes[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(bytes)
}

fn check_range(offset: u16, len: usize) -> io::Result<()> {
    if usize::from(offset) + len > PAGE {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write extends past the page",
        ))
    } else {
        Ok(())
    }
}

impl GroupCommit {
    /// `batch` is how many contiguous records to sync while writers keep the queue full.
    pub fn start(store: Arc<Store>, batch: usize) -> io::Result<Self> {
        let batch = batch.max(1);
        let start = store.next_lsn();
        let shared = Arc::new(Shared {
            store,
            queue: MpmcQueue::new(1024),
            next_lsn: AtomicU64::new(start),
            flushed: AtomicU64::new(start.saturating_sub(1)),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            syncs: AtomicU64::new(0),
            batch,
        });
        let committer_shared = Arc::clone(&shared);
        let committer = thread::Builder::new()
            .name("wal-commit".into())
            .spawn(move || commit_loop(committer_shared))?;
        Ok(Self {
            shared,
            committer: Some(committer),
        })
    }

    pub fn writer(&self) -> CommitHandle {
        CommitHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn syncs(&self) -> u64 {
        self.shared.syncs.load(Ordering::Relaxed)
    }

    /// Writers must have returned from [`CommitHandle::write`] first.
    pub fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(handle) = self.committer.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for GroupCommit {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl CommitHandle {
    /// Queue a record and return its LSN. Durability is [`Self::wait`].
    ///
    /// Waiting on every record keeps only one record per writer in flight, and
    /// the committer then never sees a full batch. Submit ahead, then wait.
    pub fn submit(&self, page: u32, offset: u16, data: &[u8]) -> io::Result<u64> {
        if data.is_empty() || data.len() > 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "group commit payload is 1..=32 bytes",
            ));
        }
        check_range(offset, data.len())?;
        let lsn = self.shared.next_lsn.fetch_add(1, Ordering::Relaxed);
        let mut job = Job {
            lsn,
            page,
            offset,
            len: data.len() as u8,
            data: [0; 32],
        };
        job.data[..data.len()].copy_from_slice(data);
        let mut spins = 0u32;
        loop {
            if self.shared.failed.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Other, "wal committer failed"));
            }
            match self.shared.queue.try_push(job) {
                Ok(()) => return Ok(lsn),
                Err(returned) => {
                    job = returned;
                    pause(&mut spins);
                }
            }
        }
    }

    pub fn wait(&self, lsn: u64) -> io::Result<()> {
        let mut spins = 0u32;
        while self.shared.flushed.load(Ordering::Acquire) < lsn {
            if self.shared.failed.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Other, "wal committer failed"));
            }
            pause(&mut spins);
        }
        Ok(())
    }

    pub fn write(&self, page: u32, offset: u16, data: &[u8]) -> io::Result<u64> {
        let lsn = self.submit(page, offset, data)?;
        self.wait(lsn)?;
        Ok(lsn)
    }
}

fn commit_loop(shared: Arc<Shared>) {
    let mut pending: BTreeMap<u64, Job> = BTreeMap::new();
    let mut expect = shared.flushed.load(Ordering::Relaxed) + 1;
    let mut spins = 0u32;
    loop {
        let popped = match shared.queue.try_pop() {
            Some(job) => {
                pending.insert(job.lsn, job);
                true
            }
            None => false,
        };
        let ready = contiguous(&pending, expect);
        if ready >= shared.batch || (ready > 0 && !popped) {
            let n = ready.min(shared.batch);
            let batch: Vec<Job> = (0..n)
                .map(|step| pending.remove(&(expect + step as u64)).expect("prefix"))
                .collect();
            if let Err(_err) = shared.store.commit_prefix(&batch) {
                shared.failed.store(true, Ordering::Release);
                return;
            }
            expect += n as u64;
            shared.flushed.store(expect - 1, Ordering::Release);
            shared.syncs.fetch_add(1, Ordering::Relaxed);
            spins = 0;
            continue;
        }
        if shared.stop.load(Ordering::Acquire) && !popped && pending.is_empty() {
            return;
        }
        if !popped {
            pause(&mut spins);
        }
    }
}

fn contiguous(pending: &BTreeMap<u64, Job>, mut expect: u64) -> usize {
    let mut n = 0usize;
    while pending.contains_key(&expect) {
        n += 1;
        expect += 1;
        if n == 1024 {
            break;
        }
    }
    n
}

fn pause(spins: &mut u32) {
    *spins = spins.wrapping_add(1);
    if *spins % 64 == 0 {
        thread::yield_now();
    } else {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct Tmp {
        path: PathBuf,
    }

    impl Tmp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "memallocator-{}-{}",
                std::process::id(),
                UNIQUE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    static UNIQUE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn replay_reads_bytes_that_were_only_in_the_wal() {
        let tmp = Tmp::new();
        let store = Store::create(&tmp.path, 4).unwrap();
        store.write_unsynced(0, 0, b"hello").unwrap();
        store.write_unsynced(1, 10, b"world").unwrap();
        store.sync().unwrap();
        drop(store);
        let store = Store::open(&tmp.path, 4).unwrap();
        assert_eq!(store.read(0, 0, 5).unwrap(), b"hello");
        assert_eq!(store.read(1, 10, 5).unwrap(), b"world");
    }

    #[test]
    fn torn_tail_is_ignored() {
        let tmp = Tmp::new();
        let store = Store::create(&tmp.path, 2).unwrap();
        store.write_unsynced(3, 1, b"kept").unwrap();
        store.sync().unwrap();
        drop(store);
        let mut wal = OpenOptions::new()
            .append(true)
            .open(tmp.path.join("wal"))
            .unwrap();
        wal.write_all(&[0xAB, 0xCD, 0xEF]).unwrap();
        drop(wal);
        let store = Store::open(&tmp.path, 2).unwrap();
        assert_eq!(store.read(3, 1, 4).unwrap(), b"kept");
        assert_eq!(store.next_lsn(), 2);
    }

    #[test]
    fn clock_evicts_and_reloads() {
        let tmp = Tmp::new();
        let store = Store::create(&tmp.path, 4).unwrap();
        for page in 0..10u32 {
            let byte = [page as u8 + 1];
            store.write_unsynced(page, 0, &byte).unwrap();
        }
        store.sync().unwrap();
        for page in 0..10u32 {
            let got = store.read(page, 0, 1).unwrap();
            assert_eq!(got[0], page as u8 + 1);
        }
    }

    #[test]
    fn group_commit_replays_after_reopen() {
        let tmp = Tmp::new();
        let store = Arc::new(Store::create(&tmp.path, 4).unwrap());
        let mut group = GroupCommit::start(Arc::clone(&store), 8).unwrap();
        let writer = group.writer();
        for page in 0..40u32 {
            writer.write(page, 0, &page.to_le_bytes()).unwrap();
        }
        group.shutdown();
        drop(group);
        drop(store);
        let store = Store::open(&tmp.path, 4).unwrap();
        for page in 0..40u32 {
            assert_eq!(store.read(page, 0, 4).unwrap(), page.to_le_bytes());
        }
    }
}
