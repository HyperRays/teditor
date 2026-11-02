//! A small shared I/O pool prevents multiple large tabs from flooding the disk.
use super::file_io::Progress;
use std::{
    io,
    sync::{
        Arc, Mutex, OnceLock,
        mpsc::{self, Receiver, Sender},
    },
};
type Task = Box<dyn FnOnce() + Send>;

pub(super) struct Worker<T> {
    receiver: Receiver<io::Result<T>>,
    pub progress: Arc<Progress>,
}
impl<T: Send + 'static> Worker<T> {
    pub fn start(
        run: impl FnOnce(&Progress) -> io::Result<T> + Send + 'static,
    ) -> io::Result<Self> {
        static POOL: OnceLock<Result<Sender<Task>, String>> = OnceLock::new();
        let queue = POOL
            .get_or_init(|| {
                let (send, receive) = mpsc::channel::<Task>();
                let receive = Arc::new(Mutex::new(receive));
                for index in 0..2 {
                    let receive = receive.clone();
                    std::thread::Builder::new()
                        .name(format!("teditor-io-{index}"))
                        .spawn(move || {
                            loop {
                                let task = receive.lock().unwrap().recv();
                                match task {
                                    Ok(task) => task(),
                                    Err(_) => break,
                                }
                            }
                        })
                        .map_err(|e| e.to_string())?;
                }
                Ok(send)
            })
            .as_ref()
            .map_err(|e| io::Error::other(e.clone()))?;
        let (send, receiver) = mpsc::channel();
        let progress = Arc::new(Progress::default());
        let control = progress.clone();
        queue
            .send(Box::new(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    control.check()?;
                    run(&control)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("File operation failed unexpectedly")));
                let _ = send.send(result);
            }))
            .map_err(|_| io::Error::other("File worker pool stopped"))?;
        Ok(Self { receiver, progress })
    }
    pub fn poll(&self) -> Option<io::Result<T>> {
        match self.receiver.try_recv() {
            Ok(value) => Some(value),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(io::Error::other("File worker disconnected")))
            }
        }
    }
}
impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.progress.cancel();
    }
}
