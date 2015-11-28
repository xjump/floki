use std::ptr;
use std::slice;
use std::thread;
use std::cmp;
use std::mem::{self, size_of};
use std::io;
use std::io::prelude::*;
use std::time::Duration;
use std::os::unix::io::{RawFd, AsRawFd};
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use nix::c_void;
use nix::sys::mman;
use spin::{Mutex as SpinLock, RwLock as SpinRwLock};
use rustc_serialize::json;
use twox_hash::XxHash;
use std::hash::Hasher;
use nix;

use config::*;
use utils::*;
use offset_index::*;
use fallocate::*;

const MAGIC_NUM: u32 = 0xF1031311u32;

pub type QueueBackendResult<T> = Result<T, QueueBackendError>;

#[derive(Debug)]
pub enum QueueBackendError {
    SegmentFull,
    SegmentFileInvalid,
    Io(io::Error),
}

impl From<io::Error> for QueueBackendError {
    fn from(from: io::Error) -> QueueBackendError {
        QueueBackendError::Io(from)
    }
}

impl From<nix::Error> for QueueBackendError {
    fn from(from: nix::Error) -> QueueBackendError {
        QueueBackendError::Io(from.into())
    }
}


#[derive(Debug, Clone)]
struct InnerMessage {
    mmap_ptr: *const MessageHeader,
}

#[derive(Debug, Clone)]
pub struct Message {
    segment: Arc<Segment>,
    inner: InnerMessage,
}

impl Message {
    pub fn id(&self) -> u64 {
        unsafe { (*self.inner.mmap_ptr).id }
    }

    pub fn timestamp(&self) -> u32 {
        unsafe { (*self.inner.mmap_ptr).timestamp }
    }

    pub fn body(&self) -> &[u8] {
        unsafe {
            slice::from_raw_parts(
                (self.inner.mmap_ptr as *const u8)
                    .offset(size_of::<MessageHeader>() as isize),
                (*self.inner.mmap_ptr).len as usize)
        }
    }

    pub fn fd(&self) -> RawFd {
        self.segment.file.as_raw_fd()
    }

    pub fn fd_offset(&self) -> u32 {
        (self.inner.mmap_ptr as usize - self.segment.file_mmap as usize) as u32
    }
}

unsafe impl Send for Message {}

#[repr(packed)]
struct MessageHeader {
    hash: u32,
    id: u64,
    timestamp: u32,
    len: u32,
}

#[derive(Debug, Default, Eq, PartialEq, RustcDecodable, RustcEncodable)]
struct SegmentCheckpoint {
    tail: u64,
    head: u64,
    sync_offset: u32,
    closed: bool,
}

#[derive(Debug, Default, RustcDecodable, RustcEncodable)]
struct QueueBackendCheckpoint {
    segments: Vec<SegmentCheckpoint>
}

#[derive(Debug)]
pub struct Segment {
    data_path: PathBuf,
    // TODO: move mmaped file abstraction
    file: File,
    file_mmap: *mut u8,
    file_size: u32,
    file_offset: u32,
    sync_offset: u32,
    // dirty_bytes: usize,
    // dirty_messages: usize,
    tail: u64,
    head: u64,
    closed: bool,
    deleted: bool,
    index: SpinLock<OffsetIndex>,
}

#[derive(Debug)]
pub struct QueueBackend {
    config: Rc<QueueConfig>,
    segments: SpinRwLock<Vec<Arc<Segment>>>,
    head: u64,
    tail: u64
}

impl Segment {
    fn gen_file_path(config: &QueueConfig, start_id: u64, ext: &str) -> PathBuf {
        config.data_directory.join(format!("{:015}.{}", start_id, ext))
    }

    fn hash_segment_message(header: &MessageHeader) -> u32 {
        let mut hasher = XxHash::with_seed(0);
        unsafe {
            hasher.write(slice::from_raw_parts(
                (header as *const _ as *const u8).offset(size_of::<u32>() as isize),
                size_of::<MessageHeader>() + header.len as usize - size_of::<u32>()
            ));
        }
        (hasher.finish() & 0xFFFFFFFF) as u32
    }

    fn create(config: &QueueConfig, start_id: u64) -> QueueBackendResult<Segment> {
        let data_path = Self::gen_file_path(config, start_id, DATA_EXTENSION);
        debug!("[{}] creating data file {:?}", config.name, data_path);
        let file = try!(OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&data_path));
        try!(fallocate(&file, 0, config.segment_size));

        let mut segment = try!(Self::new(config, file, data_path, start_id));
        unsafe {
            *(segment.file_mmap as *mut u32) = MAGIC_NUM;
        }
        segment.file_offset = size_of::<u32>() as u32;

        Ok(segment)
    }

    fn open(config: &QueueConfig, checkpoint: &SegmentCheckpoint) -> QueueBackendResult<Segment> {
        let data_path = Self::gen_file_path(config, checkpoint.tail, DATA_EXTENSION);
        debug!("[{}] opening data file {:?}", config.name, data_path);
        let file = try!(OpenOptions::new().read(true).write(true).open(&data_path));
        let mut segment = try!(Self::new(config, file, data_path, checkpoint.tail));
        try!(segment.recover(checkpoint));
        Ok(segment)
    }

    fn new(config: &QueueConfig, file: File, data_path: PathBuf, start_id: u64) -> QueueBackendResult<Segment> {
        let file_size = try!(file.metadata()).len();
        if file_size <= 4 {
            return Err(QueueBackendError::SegmentFileInvalid)
        }

        let file_mmap = try!(mman::mmap(
            ptr::null_mut(), file_size,
            mman::PROT_READ | mman::PROT_WRITE, mman::MAP_SHARED,
            file.as_raw_fd(), 0)) as *mut u8;
        try!(mman::madvise(
            file_mmap as *mut c_void,
            file_size,
            mman::MADV_SEQUENTIAL));

        Ok(Segment {
            data_path: data_path,
            file: file,
            file_mmap: file_mmap,
            file_size: file_size as u32,
            file_offset: 0,
            sync_offset: 0,
            tail: start_id,
            head: start_id,
            // dirty_messages: 0,
            // dirty_bytes: 0,
            closed: false,
            deleted: false,
            index: SpinLock::new(OffsetIndex::new(start_id))
        })
    }
    
    fn get(&self, id: u64) -> Result<InnerMessage, ()> {
        let message_offset = if let Some(message_offset) = self.index.lock().get_offset(id) {
            message_offset
        } else {
            return Err(())
        };

        let header: &MessageHeader = unsafe {
            mem::transmute(self.file_mmap.offset(message_offset as isize))
        };

        // check id and possible overflow
        assert!(message_offset <= self.file_offset,
            "Corrupt file, message start offset {} is past file offset {}", message_offset, self.file_offset);
        let message_end_offset = message_offset + size_of::<MessageHeader>() as u32 + header.len as u32;
        assert!(message_end_offset <= self.file_offset,
            "Corrupt file, message end offset {} is past file offset {}", message_end_offset, self.file_offset);
        assert!(header.id == id,
            "Corrupt file, ids don't match {} {} at offset {}", header.id, id, message_offset);
        let mmap_ptr = unsafe { self.file_mmap.offset(message_offset as isize + size_of::<MessageHeader>() as isize) };

        let message = InnerMessage {
            mmap_ptr: header,
        };

        Ok(message)
    }

    fn push(&mut self, body: &[u8], clock: u32) -> QueueBackendResult<u64> {
        let message_total_len = size_of::<MessageHeader>() + body.len();

        if message_total_len as u32 > self.file_size - self.file_offset {
            self.closed = true;
            return Err(QueueBackendError::SegmentFull)
        }

        let id = self.head;

        unsafe {
            let header: &mut MessageHeader = mem::transmute(self.file_mmap.offset(self.file_offset as isize));
            header.id = self.head;
            header.timestamp = clock;
            header.len = body.len() as u32;
            ptr::copy_nonoverlapping(
                body.as_ptr(),
                self.file_mmap.offset(self.file_offset as isize + size_of::<MessageHeader>() as isize),
                body.len());
            header.hash = Self::hash_segment_message(header);
        }

        self.file_offset += message_total_len as u32;
        self.head += 1;
        self.index.lock().push_offset(id, self.file_offset - message_total_len as u32);
        // self.dirty_bytes += message_total_len;
        // self.dirty_messages += 1;

        Ok(id)
    }

    fn sync(&mut self, full: bool) {
        let sync_offset = if full { self.file_offset } else { self.file_offset.saturating_sub(1024 * 1024) };
        if sync_offset > 0 && sync_offset > self.sync_offset {
            mman::msync(self.file_mmap as *mut c_void, sync_offset as u64, mman::MS_SYNC).unwrap();
            self.sync_offset = sync_offset;
        }
    }

    fn recover(&mut self, checkpoint: &SegmentCheckpoint) -> QueueBackendResult<()> {
        debug!("[{:?}] checkpoint loaded: {:?}", self.data_path, checkpoint);
        assert_eq!(self.tail, checkpoint.tail);
        self.tail = checkpoint.tail;
        self.head = checkpoint.tail;
        self.sync_offset = checkpoint.sync_offset;
        self.closed = checkpoint.closed;

        try!(mman::madvise(
            self.file_mmap as *mut c_void,
            self.sync_offset as u64,
            mman::MADV_WILLNEED));

        self.file_offset = size_of::<u32>() as u32;
        unsafe {
            if *(self.file_mmap as *mut u32) != MAGIC_NUM {
                warn!("[{:?}] incorrect magic number", self.data_path);
                *(self.file_mmap as *mut u32) = MAGIC_NUM
            }
        }

        let header_size = size_of::<MessageHeader>() as u32;
        let mut locked_index = self.index.lock();
        while self.file_offset + header_size < self.file_size {
            let header: &MessageHeader = unsafe {
                mem::transmute(self.file_mmap.offset(self.file_offset as isize))
            };
            if header.id != self.head {
                warn!("[{:?}] expected id {} got {} when recovering @{}",
                    self.data_path, self.head, header.id, self.file_offset);
                break
            }
            if self.file_offset + header_size + header.len < self.file_size {
                if header.hash != Self::hash_segment_message(header) {
                    warn!("[{:?}] corrupt message with id {} when recovering @{}",
                        self.data_path, header.id, self.file_offset);
                    break
                }
            } else {
                warn!("[{:?}] message with id {} would overflow file @{}",
                    self.data_path, header.id, self.file_offset);
                break
            }
            locked_index.push_offset(header.id, self.file_offset);
            self.file_offset += header_size + header.len;
            self.head += 1;
        }

        self.sync_offset = self.file_offset;
        Ok(())
    }

    fn checkpoint(&mut self, full: bool) -> SegmentCheckpoint {
        // FIXME: reset and log stats
        let mut checkpoint = SegmentCheckpoint {
            tail: self.tail,
            head: self.head,
            sync_offset: self.sync_offset,
            closed: self.closed,
        };
        self.sync(full);
        // update sync_offset
        checkpoint.sync_offset = self.sync_offset;
        checkpoint
    }

    fn purge(&mut self) {
        assert!(!self.deleted);
        self.deleted = true;
    }

    #[allow(mutable_transmutes)]
    pub fn as_mut(&self) -> &mut Self {
        unsafe { mem::transmute(self) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        let mmap = self.file_mmap as *mut c_void;
        mman::madvise(mmap, self.file_size as u64, mman::MADV_DONTNEED).unwrap();
        mman::munmap(mmap, self.file_size as u64).unwrap();
        if self.deleted {
            remove_file_if_exist(&self.data_path).unwrap();
        }
    }
}

impl QueueBackend {
    pub fn new(config: Rc<QueueConfig>, recover: bool) -> QueueBackend {
        let mut backend = QueueBackend {
            config: config,
            segments: SpinRwLock::new(Vec::new()),
            head: 1,
            tail: 1
        };
        if recover {
            backend.recover();
        }
        backend
    }

    pub fn segments_count(&self) -> usize {
        self.segments.read().len()
    }

    pub fn tail(&self) -> u64 {
        self.tail
    }

    pub fn head(&self) -> u64 {
        self.head
    }

    fn find_segment(&self, id: u64) -> Option<Arc<Segment>> {
        for segment in self.segments.read().iter() {
            if id < segment.head && segment.tail < segment.head {
                return Some(segment.clone())
            }
        }
        None
    }

    /// Put a message at the end of the Queue, return the message id if succesfull
    /// Note: it's the caller responsability to serialize write calls
    pub fn push(&mut self, body: &[u8], timestamp: u32) -> QueueBackendResult<u64> {
        let result = if let Some(segment) = self.segments.read().last() {
            segment.as_mut().push(body, timestamp)
        } else {
            Err(QueueBackendError::SegmentFull)
        };

        match result {
            Ok(id) => {
                assert_eq!(id, self.head);
                self.head += 1;
                return result
            },
            Err(QueueBackendError::SegmentFull) => (),
            Err(_) => return result
        }

        // create a new segment
        let segment = Arc::new(try!(Segment::create(&self.config, self.head)));
        self.segments.write().push(segment.clone());

        let id = try!(segment.as_mut().push(body, timestamp));
        assert_eq!(id, self.head);
        self.head += 1;
        Ok(id)
    }

    /// Get a new message with the specified id
    /// if not possible, return the next available message
    pub fn get(&mut self, id: u64) -> Option<Message> {
        if let Some(segment) = self.find_segment(id) {
            if let Ok(inner) = segment.get(cmp::max(id, segment.tail)) { 
                return Some(Message {
                    inner: inner,
                    segment: segment,
                })
            }
        }
        None
    }

    pub fn purge(&mut self) {
        self.tail = self.head;
        self.segments.write().last_mut().map(|last| last.as_mut().closed = true);
    }

    fn wait_delete_segment(segment: Arc<Segment>) {
        while Arc::strong_count(&segment) > 1 {
            thread::sleep(Duration::from_millis(100));
        }
        segment.as_mut().purge()
    }

    pub fn delete(&mut self) {
        let path = self.config.data_directory.join(BACKEND_CHECKPOINT_FILE);
        remove_file_if_exist(&path).unwrap();
        let mut locked_segments = self.segments.write();
        for segment in locked_segments.drain(..) {
            Self::wait_delete_segment(segment);
        }
    }

    fn recover(&mut self) {
        let path = self.config.data_directory.join(BACKEND_CHECKPOINT_FILE);
        let backend_checkpoint: QueueBackendCheckpoint = match File::open(path) {
            Ok(mut file) => {
                let mut contents = String::new();
                let _ = file.read_to_string(&mut contents);
                let checkpoint_result = json::decode(&contents);
                match checkpoint_result {
                    Ok(state) => state,
                    Err(error) => {
                        error!("[{}] error parsing checkpoint information: {}",
                            self.config.name, error);
                        return;
                    }
                }
            }
            Err(error) => {
                warn!("[{}] error reading checkpoint information: {}",
                    self.config.name, error);
                return;
            }
        };

        info!("[{}] checkpoint loaded: {:?}", self.config.name, backend_checkpoint);

        let mut locked_segments = self.segments.write();
        for segment_checkpoint in &backend_checkpoint.segments {
            let segment = match Segment::open(&self.config, segment_checkpoint) {
                Ok(inner_segment) => Arc::new(inner_segment),
                Err(error) => {
                    warn!("[{}] error opening segment from checkpoint {:?}: {:?}",
                        self.config.name, segment_checkpoint, error);
                    continue
                },
            };
            if !segment.closed && locked_segments.last_mut().is_some() {
                // make sure we only have one open segment, the last
                locked_segments.last_mut().unwrap().as_mut().closed = true;
            }
            locked_segments.push(segment);
        }

        if let Some(first_file) = locked_segments.first() {
            self.tail = first_file.tail
        }

        if let Some(last_file) = locked_segments.last() {
            self.head = last_file.head;
        }
    }

    pub fn checkpoint(&mut self, full: bool) {
        let segments_copy = self.segments.read().clone();
        let file_checkpoints: Vec<_> = segments_copy.into_iter().filter_map(|segment| {
            if segment.tail >= self.tail {
                Some(segment.as_mut().checkpoint(full))
            } else {
                None
            }
        }).collect();

        let checkpoint = QueueBackendCheckpoint {
            segments: file_checkpoints
        };

        let tmp_path = self.config.data_directory.join(TMP_BACKEND_CHECKPOINT_FILE);
        let result = File::create(&tmp_path)
            .and_then(|mut file| {
                try!(write!(file, "{}", json::as_pretty_json(&checkpoint)));
                file.sync_data()
            }).and_then(|_| {
                let final_path = tmp_path.with_file_name(BACKEND_CHECKPOINT_FILE);
                fs::rename(tmp_path, final_path)
            });

        match result {
            Ok(_) => {
                info!("[{}] checkpointed: {:?}", self.config.name, checkpoint.segments);
            }
            Err(error) => {
                warn!("[{}] error writing checkpoint information: {}",
                    self.config.name, error);
            }
        }
    }

    pub fn gc(&mut self, smallest_tail: u64) {
        let mut gc_seg_count = 0;
        // collect until the first still open or with head > smallest_tail
        for segment in &*self.segments.read() {
            if ! segment.closed || segment.head > smallest_tail {
                break
            }
            gc_seg_count += 1;
        }
        if gc_seg_count != 0 {
            info!("[{}] {} segments available for gc", self.config.name, gc_seg_count);
            // purge them
            let mut gc_segments: Vec<_> = Vec::with_capacity(gc_seg_count);
            {
                let mut locked_segments = self.segments.write();
                gc_segments.extend(locked_segments.drain(..gc_seg_count));
            }
            self.tail = gc_segments.last().unwrap().head;
            for segment in gc_segments {
                Self::wait_delete_segment(segment)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::*;
    use std::fs;
    use std::thread;
    use std::rc::Rc;
    use utils;

    fn get_backend_opt(name: &str, recover: bool) -> QueueBackend {
        let mut server_config = ServerConfig::read();
        server_config.data_directory = "./test_data".into();
        server_config.segment_size = 4 * 1024 * 1024;
        let mut queue_config = server_config.new_queue_config(name);
        queue_config.time_to_live = 1;
        utils::create_dir_if_not_exist(&queue_config.data_directory).unwrap();
        QueueBackend::new(Rc::new(queue_config), recover)
    }

    fn get_backend() -> QueueBackend {
        get_backend_opt(thread::current().name().unwrap(), false)
    }

    fn get_backend_recover() -> QueueBackend {
        get_backend_opt(thread::current().name().unwrap(), true)
    }

    fn gen_message() -> &'static [u8] {
        return b"333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333";
    }

    #[test]
    fn test_corrupt_files() {
        let mut backend = get_backend();
        let mut num_writes = 0;
        while backend.segments_count() < 3 {
            backend.push(gen_message(), 0).unwrap();
            num_writes += 1;
        }
        backend.checkpoint(true);
        {
            // zero second half of first file
            let mut segment = backend.segments.read()[0].clone();
            segment.as_mut().file.set_len(segment.file_size as u64 / 2).unwrap();
            segment.as_mut().file.set_len(segment.file_size as u64).unwrap();
            // nuke the second
            segment = backend.segments.read()[1].clone();
            segment.as_mut().file.set_len(0).unwrap();
            segment.as_mut().file.set_len(segment.file_size as u64).unwrap();
        }

        let mut backend = get_backend_recover();
        let mut num_reads = 0;
        let mut id = 1;
        let mut holes = 0;
        while let Some(message) = backend.get(id) {
            num_reads += 1;
            if message.id() != id {
                holes += 1
            }
            id = message.id() + 1;
        }
        assert_eq!(id, backend.head());
        assert_eq!(num_reads, num_writes / 4 + 1);
        assert_eq!(holes, 1);

        {
            // truncate the third
            let segment = backend.segments.read()[2].clone();
            segment.as_mut().file.set_len(0).unwrap();
        }

        backend = get_backend_recover();
        num_reads = 0;
        id = 1;
        holes = 0;
        while let Some(message) = backend.get(id) {
            num_reads += 1;
            if message.id() != id {
                holes += 1
            }
            id = message.id() + 1;
        }
        assert_eq!(id, backend.segments.read()[0].head);
        assert_eq!(num_reads, num_writes / 4);
        assert_eq!(holes, 0);

        // push to the third file, that we nuked
        let prev_reads = num_reads;
        backend.push(gen_message(), 0).unwrap() == num_writes + 1;
        backend.push(gen_message(), 0).unwrap() == num_writes + 2;

        num_reads = 0;
        id = 1;
        holes = 0;
        while let Some(message) = backend.get(id) {
            num_reads += 1;
            if message.id() != id {
                holes += 1
            }
            id = message.id() + 1;
        }
        assert_eq!(id, backend.head());
        assert_eq!(num_reads, prev_reads + 2);
        assert_eq!(holes, 1);
    }

    #[test]
    fn test_missing_file() {
        let mut num_writes = 0;
        let file_paths: Vec<_> = {
            let mut backend = get_backend();
            while backend.segments_count() < 3 {
                backend.push(gen_message(), 0).unwrap();
                num_writes += 1;
            }
            backend.checkpoint(true);

            let locked_segments = backend.segments.read();
            locked_segments.iter().map(|s| s.data_path.clone()).collect()
        };

        for (segment_num, file_path) in file_paths.iter().enumerate() {
            fs::remove_file(&file_path).unwrap();

            let mut backend = get_backend_recover();
            let mut num_reads = 0;
            let mut id = 1;
            let mut holes = 0;
            while let Some(message) = backend.get(id) {
                num_reads += 1;
                if message.id() != id {
                    holes += 1
                }
                id = message.id() + 1;
            }
            assert_eq!(id, backend.head());
            match segment_num {
                0 => {
                    assert_eq!(num_reads, (num_writes - 1) / 2 + 1);
                    assert_eq!(holes, 1);
                },
                1 => {
                    assert_eq!(num_reads, 1);
                    assert_eq!(holes, 1);
                },
                2 => {
                    assert_eq!(num_reads, 0);
                    assert_eq!(holes, 0);
                },
                _ => unreachable!()
            }
        }
    }
}
