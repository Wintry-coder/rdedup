

use dangerous_option::DangerousOption as AutoOption;

use num_cpus;
use sgdata::SGData;
use slog;
use slog::{Level, Logger};
use slog_perf::TimeReporter;
use std;
use std::{fs, io, mem, thread};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use two_lock_queue;
use walkdir::WalkDir;
use std::cell::RefCell;

mod local;
pub(crate) use self::local::Local;

pub(crate) trait Backend: Send + Sync {
    fn lock_exclusive(&self) -> io::Result<fs::File>;
    fn lock_shared(&self) -> io::Result<fs::File>;
    fn new_instance(&self) -> Box<BackendInstance>;
}

pub(crate) trait BackendInstance: Send {
    fn remove_dir_all(&mut self, path: PathBuf) -> io::Result<()>;

    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
    ) -> io::Result<()>;


    fn write(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
    ) -> io::Result<()>;


    fn read(&mut self, path: PathBuf) -> io::Result<SGData>;

    fn remove(&mut self, path: PathBuf) -> io::Result<()>;

    fn read_metadata(&mut self, path: PathBuf) -> io::Result<Metadata>;
    fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>>;
}

struct WriteArgs {
    path: PathBuf,
    data: SGData,
    idempotent: bool,
    complete_tx: Option<mpsc::Sender<io::Result<()>>>,
}

pub(crate) struct Metadata {
    len: u64,
    is_file: bool,
}

/// Message sent to a worker pool
enum Message {
    Write(WriteArgs),
    Read(PathBuf, mpsc::Sender<io::Result<SGData>>),
    ReadMetadata(PathBuf, mpsc::Sender<io::Result<Metadata>>),
    List(PathBuf, mpsc::Sender<io::Result<Vec<PathBuf>>>),
    ListRecursively(PathBuf, mpsc::Sender<io::Result<Vec<PathBuf>>>),
    Remove(PathBuf, mpsc::Sender<io::Result<()>>),
    RemoveDirAll(PathBuf, mpsc::Sender<io::Result<()>>),
    Rename(PathBuf, PathBuf, mpsc::Sender<io::Result<()>>),
}

/// A handle to a async-io worker pool
///
/// This object abstracts away file system asynchronous operations. In the
/// future, it will be supplied/parametrized by an underlying logic,
/// implementing
/// different backends (filesystem, protocols to remote locations etc).
#[derive(Clone)]
pub struct AsyncIO {
    shared: Arc<AsyncIOShared>,
    tx: AutoOption<two_lock_queue::Sender<Message>>,
}

impl AsyncIO {
    pub(crate) fn new(
        root_path: PathBuf,
        backend: Box<Backend + Send + Sync>,
        log: Logger,
    ) -> Self {
        let thread_num = 4 * num_cpus::get();
        let (tx, rx) = two_lock_queue::channel(thread_num);

        let shared = AsyncIOThreadShared::new();

        let join = (0..thread_num)
            .map(|_| {
                let rx = rx.clone();
                let shared = shared.clone();
                let log = log.clone();
                let root_path = root_path.clone();
                let backend = backend.new_instance();
                thread::spawn(move || {
                    let mut thread =
                        AsyncIOThread::new(root_path, shared, rx, backend, log);
                    thread.run();
                })
            })
            .collect();

        drop(rx);

        let shared = AsyncIOShared {
            join: join,
            log: log.clone(),
            stats: shared.clone(),
            backend: backend,
        };

        AsyncIO {
            shared: Arc::new(shared),
            tx: AutoOption::new(tx),
        }
    }


    pub(crate) fn lock_exclusive(&self) -> io::Result<fs::File> {
        self.shared.backend.lock_exclusive()
    }

    pub(crate) fn lock_shared(&self) -> io::Result<fs::File> {
        self.shared.backend.lock_shared()
    }

    pub fn stats(&self) -> AsyncIOThreadShared {
        self.shared.stats.clone()
    }

    pub fn list(&self, path: PathBuf) -> AsyncIOResult<Vec<PathBuf>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::List(path, tx))
            .expect("channel send failed");
        AsyncIOResult { rx: rx }
    }

    pub fn list_recursively(
        &self,
        path: PathBuf,
    ) -> Box<Iterator<Item = io::Result<PathBuf>>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::ListRecursively(path, tx))
            .expect("channel send failed");

        let iter = rx.into_iter().flat_map(|batch| match batch {
            Ok(batch) => Box::new(batch.into_iter().map(Ok))
                as Box<Iterator<Item = io::Result<PathBuf>>>,
            Err(e) => Box::new(Some(Err(e)).into_iter())
                as Box<Iterator<Item = io::Result<PathBuf>>>,
        });
        Box::new(iter)
    }

    pub fn write(&self, path: PathBuf, sg: SGData) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::Write(WriteArgs {
                path: path,
                data: sg,
                idempotent: false,
                complete_tx: Some(tx),
            }))
            .expect("channel send failed");
        AsyncIOResult { rx: rx }
    }

    #[allow(dead_code)]
    pub fn write_idempotent(
        &self,
        path: PathBuf,
        sg: SGData,
    ) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::Write(WriteArgs {
                path: path,
                data: sg,
                idempotent: true,
                complete_tx: Some(tx),
            }))
            .expect("channel send failed");
        AsyncIOResult { rx: rx }
    }

    /// Will panic the worker thread if fails, but does not require
    /// managing the result
    #[allow(dead_code)]
    pub fn write_checked(&self, path: PathBuf, sg: SGData) {
        self.tx
            .send(Message::Write(WriteArgs {
                path: path,
                data: sg,
                idempotent: false,
                complete_tx: None,
            }))
            .expect("channel send failed");
    }

    pub fn write_checked_idempotent(&self, path: PathBuf, sg: SGData) {
        self.tx
            .send(Message::Write(WriteArgs {
                path: path,
                data: sg,
                idempotent: true,
                complete_tx: None,
            }))
            .expect("channel send failed");
    }

    pub fn read(&self, path: PathBuf) -> AsyncIOResult<SGData> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::Read(path, tx))
            .expect("channel send failed");
        AsyncIOResult { rx: rx }
    }

    pub(crate) fn read_metadata(
        &self,
        path: PathBuf,
    ) -> AsyncIOResult<Metadata> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::ReadMetadata(path, tx))
            .expect("channel send failed");
        AsyncIOResult { rx: rx }
    }

    pub fn remove(&self, path: PathBuf) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::Remove(path, tx))
            .expect("channel send failed");
        AsyncIOResult { rx: rx }
    }

    pub fn remove_dir_all(&self, path: PathBuf) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::RemoveDirAll(path, tx))
            .expect("channel send failed");
        AsyncIOResult { rx: rx }
    }

    pub fn rename(&self, src: PathBuf, dst: PathBuf) -> AsyncIOResult<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::Rename(src, dst, tx))
            .expect("channel send failed");
        AsyncIOResult { rx: rx }
    }
}

impl Drop for AsyncIO {
    fn drop(&mut self) {
        // It is important that the tx is dropped before `shared` is.
        // Otherwise join on worker threads will hang, as they are never
        // going to receive termination.
        AutoOption::take_unchecked(&mut self.tx);
    }
}

/// Arc-ed shared `AsyncIO` data
pub struct AsyncIOShared {
    join: Vec<thread::JoinHandle<()>>,
    log: slog::Logger,
    stats: AsyncIOThreadShared,
    backend: Box<Backend + Send + Sync>,
}

impl Drop for AsyncIOShared {
    fn drop(&mut self) {
        trace!(self.log, "Waiting for all threads to finish");
        for join in self.join.drain(..) {
            join.join().expect("AsyncIO worker thread panicked")
        }
    }
}

/// A result of async io operation
///
/// It behaves a bit like a future/promise. It is happening
/// in the background, and only calling `wait` will make sure
/// the operations completed and return result.
#[must_use]
pub struct AsyncIOResult<T> {
    rx: mpsc::Receiver<io::Result<T>>,
}

impl<T> AsyncIOResult<T> {
    /// Block until result arrives
    pub fn wait(self) -> io::Result<T> {
        self.rx.recv().expect("No `AsyncIO` thread response")
    }
}


#[derive(Clone, Debug)]
pub struct WriteStats {
    pub new_chunks: usize,
    pub new_bytes: u64,
}


struct AsyncIOSharedInner {
    write_stats: WriteStats,
    in_progress: HashSet<PathBuf>,
}

impl Drop for AsyncIOSharedInner {
    fn drop(&mut self) {
        debug_assert!(self.in_progress.is_empty());
    }
}

#[derive(Clone)]
pub struct AsyncIOThreadShared {
    inner: Arc<Mutex<AsyncIOSharedInner>>,
}

impl AsyncIOThreadShared {
    pub fn new() -> Self {
        let inner = AsyncIOSharedInner {
            write_stats: WriteStats {
                new_bytes: 0,
                new_chunks: 0,
            },
            in_progress: Default::default(),
        };

        AsyncIOThreadShared {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn get_stats(&self) -> WriteStats {
        let sh = self.inner.lock().unwrap();
        sh.write_stats.clone()
    }
}

struct AsyncIOThread {
    root_path: PathBuf,
    shared: AsyncIOThreadShared,
    rx: two_lock_queue::Receiver<Message>,
    log: Logger,
    time_reporter: TimeReporter,
    backend: RefCell<Box<BackendInstance>>,
}

struct PendingGuard<'a, 'b>(&'a AsyncIOThread, &'b PathBuf);

impl<'a, 'b> Drop for PendingGuard<'a, 'b> {
    fn drop(&mut self) {
        let mut sh = self.0.shared.inner.lock().unwrap();
        sh.in_progress.remove(self.1);
    }
}

impl AsyncIOThread {
    fn new(
        root_path: PathBuf,
        shared: AsyncIOThreadShared,
        rx: two_lock_queue::Receiver<Message>,
        backend: Box<BackendInstance>,
        log: Logger,
    ) -> Self {
        let t = TimeReporter::new_with_level(
            "chunk-writer",
            log.clone(),
            Level::Debug,
        );
        AsyncIOThread {
            root_path: root_path,
            log: log.new(o!("module" => "asyncio")),
            shared: shared,
            rx: rx,
            time_reporter: t,
            backend: RefCell::new(backend),
        }
    }

    pub fn run(&mut self) {
        loop {
            self.time_reporter.start("rx");

            if let Ok(msg) = self.rx.recv() {
                match msg {
                    Message::Write(WriteArgs {
                        path,
                        data,
                        idempotent,
                        complete_tx,
                    }) => self.write(path, data, idempotent, complete_tx),
                    Message::Read(path, tx) => self.read(path, tx),
                    Message::ReadMetadata(path, tx) => {
                        self.read_metadata(path, tx)
                    }
                    Message::List(path, tx) => self.list(path, tx),
                    Message::ListRecursively(path, tx) => {
                        self.list_recursively(path, tx)
                    }
                    Message::Remove(path, tx) => self.remove(path, tx),
                    Message::RemoveDirAll(path, tx) => {
                        self.remove_dir_all(path, tx)
                    }
                    Message::Rename(src_path, dst_path, tx) => {
                        self.rename(src_path, dst_path, tx)
                    }
                }
            } else {
                break;
            }
        }
    }

    fn write_inner(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
    ) -> io::Result<()> {
        self.time_reporter.start("processing write");
        // check `in_progress` and add atomically
        // if not already there
        loop {
            let mut sh = self.shared.inner.lock().unwrap();

            if sh.in_progress.contains(&path) {
                if idempotent {
                    return Ok(());
                } else {
                    // a bit lame, but will do, since this should not really
                    // happen in practice anyway
                    drop(sh);
                    thread::sleep(std::time::Duration::from_millis(1000));
                }
            } else {
                sh.in_progress.insert(path.clone());
                break;
            }
        }

        let len = sg.len();
        let res = self.backend
            .borrow_mut()
            .write(path.clone(), sg, idempotent);
        {
            let mut sh = self.shared.inner.lock().unwrap();
            sh.in_progress.remove(&path);
            sh.write_stats.new_bytes += len as u64;
            sh.write_stats.new_chunks += 1;
        }

        res
    }

    fn write(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
        tx: Option<mpsc::Sender<io::Result<()>>>,
    ) {
        trace!(self.log, "write"; "path" => %path.display());

        let path = self.root_path.join(path);

        self.time_reporter.start("read");
        let res = {
            let _guard = self.pending_wait_and_insert(&path);
            self.backend
                .borrow_mut()
                .write(path.clone(), sg, idempotent)
        };

        if let Some(tx) = tx {
            self.time_reporter.start("write send response");
            tx.send(res).expect("send failed")
        }
    }

    fn pending_wait_and_insert<'a, 'path>(
        &'a self,
        path: &'path PathBuf,
    ) -> PendingGuard<'a, 'path> {
        loop {
            let mut sh = self.shared.inner.lock().unwrap();

            if sh.in_progress.contains(path) {
                // a bit lame, but will do, since this should not really
                // happen in practice anyway
                drop(sh);
                thread::sleep(std::time::Duration::from_millis(1000));
            } else {
                sh.in_progress.insert(path.clone());
                break;
            }
        }
        PendingGuard(self, path)
    }

    fn read(&mut self, path: PathBuf, tx: mpsc::Sender<io::Result<SGData>>) {
        trace!(self.log, "read"; "path" => %path.display());

        let path = self.root_path.join(path);

        self.time_reporter.start("read");
        let res = {
            let _guard = self.pending_wait_and_insert(&path);
            self.backend.borrow_mut().read(path.clone())
        };
        self.time_reporter.start("read send response");
        tx.send(res).expect("send failed")
    }


    fn read_metadata(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Metadata>>,
    ) {
        trace!(self.log, "read-metadata"; "path" => %path.display());

        self.time_reporter.start("read-metadata");
        let path = self.root_path.join(path);
        let res = {
            let _guard = self.pending_wait_and_insert(&path);
            self.backend.borrow_mut().read_metadata(path.clone())
        };

        self.time_reporter.start("read send response");
        tx.send(res).expect("send failed")
    }

    fn list(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        trace!(self.log, "list"; "path" => %path.display());

        let path = self.root_path.join(path);

        self.time_reporter.start("list");
        let res = self.backend.borrow_mut().list(path);
        self.time_reporter.start("list send response");
        tx.send(res).expect("send failed")
    }

    fn list_recursively(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        trace!(self.log, "list"; "path" => %path.display());
        self.time_reporter.start("list");

        let path = self.root_path.join(path);

        if !path.exists() {
            return;
        }

        let mut v = Vec::with_capacity(128);

        for path in WalkDir::new(path) {
            match path {
                Ok(path) => {
                    if !path.file_type().is_file() {
                        continue;
                    }
                    v.push(path.path().into());
                    if v.len() > 100 {
                        tx.send(Ok(mem::replace(&mut v, vec![])))
                            .expect("send failed")
                    }
                }
                Err(e) => tx.send(Err(e.into())).expect("send failed"),
            }
        }
        if !v.is_empty() {
            tx.send(Ok(v)).expect("send failed")
        }
    }

    fn remove(&mut self, path: PathBuf, tx: mpsc::Sender<io::Result<()>>) {
        trace!(self.log, "remove"; "path" => %path.display());

        let path = self.root_path.join(path);

        self.time_reporter.start("remove");
        let res = {
            let _guard = self.pending_wait_and_insert(&path);
            self.backend.borrow_mut().remove(path.clone())
        };
        self.time_reporter.start("remove send response");
        tx.send(res).expect("send failed")
    }


    fn remove_dir_all(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<()>>,
    ) {
        trace!(self.log, "remove-dir-all"; "path" => %path.display());

        let path = self.root_path.join(path);

        self.time_reporter.start("remove-dir-all");
        let res = self.backend.borrow_mut().remove_dir_all(path);

        self.time_reporter.start("remove send response");
        tx.send(res).expect("send failed")
    }

    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
        tx: mpsc::Sender<io::Result<()>>,
    ) {
        trace!(self.log, "rename"; "src-path" => %src_path.display(), "dst-path"
               => %dst_path.display());

        let src_path = self.root_path.join(src_path);
        let dst_path = self.root_path.join(dst_path);

        self.time_reporter.start("rename");
        let res = {
            let _guard = self.pending_wait_and_insert(&src_path);
            let _guard = self.pending_wait_and_insert(&dst_path);
            self.backend
                .borrow_mut()
                .rename(src_path.clone(), dst_path.clone())
        };
        self.time_reporter.start("remove send response");
        tx.send(res).expect("send failed")
    }
}
