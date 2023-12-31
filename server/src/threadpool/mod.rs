#![allow(dead_code)]
extern crate num_cpus;

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

trait FnBox {
    fn call_box(self: Box<Self>);
}

impl<F: FnOnce()> FnBox for F {
    fn call_box(self: Box<F>) {
        (*self)()
    }
}

type Thunk<'a> = Box<dyn FnBox + Send + 'a>;

struct Sentinel<'a> {
    shared_data: &'a Arc<ThreadPoolSharedData>,
    active: bool,
}

impl<'a> Sentinel<'a> {
    fn new(shared_data: &'a Arc<ThreadPoolSharedData>) -> Sentinel<'a> {
        Sentinel {
            shared_data,
            active: true,
        }
    }

    /// Cancel and destroy this sentinel.
    fn cancel(mut self) {
        self.active = false;
    }
}

impl<'a> Drop for Sentinel<'a> {
    fn drop(&mut self) {
        if self.active {
            self.shared_data.active_count.fetch_sub(1, Ordering::SeqCst);
            if thread::panicking() {
                self.shared_data.panic_count.fetch_add(1, Ordering::SeqCst);
            }
            self.shared_data.no_work_notify_all();
            spawn_in_pool(self.shared_data.clone())
        }
    }
}

#[derive(Clone, Default)]
pub struct Builder {
    num_threads: Option<usize>,
    thread_name: Option<String>,
    thread_stack_size: Option<usize>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder {
            num_threads: None,
            thread_name: None,
            thread_stack_size: None,
        }
    }

    pub fn num_threads(mut self, num_threads: usize) -> Builder {
        assert!(num_threads > 0);
        self.num_threads = Some(num_threads);
        self
    }

    pub fn thread_name(mut self, name: String) -> Builder {
        self.thread_name = Some(name);
        self
    }

    pub fn thread_stack_size(mut self, size: usize) -> Builder {
        self.thread_stack_size = Some(size);
        self
    }

    pub fn build(self) -> ThreadPool {
        let (tx, rx) = channel::<Thunk<'static>>();

        let num_threads = self.num_threads.unwrap_or_else(num_cpus::get);

        let shared_data = Arc::new(ThreadPoolSharedData {
            name: self.thread_name,
            job_receiver: Mutex::new(rx),
            empty_condvar: Condvar::new(),
            empty_trigger: Mutex::new(()),
            join_generation: AtomicUsize::new(0),
            queued_count: AtomicUsize::new(0),
            active_count: AtomicUsize::new(0),
            max_thread_count: AtomicUsize::new(num_threads),
            panic_count: AtomicUsize::new(0),
            stack_size: self.thread_stack_size,
        });

        // Threadpool threads
        for _ in 0..num_threads {
            spawn_in_pool(shared_data.clone());
        }

        ThreadPool {
            jobs: tx,
            shared_data,
        }
    }
}

struct ThreadPoolSharedData {
    name: Option<String>,
    job_receiver: Mutex<Receiver<Thunk<'static>>>,
    empty_trigger: Mutex<()>,
    empty_condvar: Condvar,
    join_generation: AtomicUsize,
    queued_count: AtomicUsize,
    active_count: AtomicUsize,
    max_thread_count: AtomicUsize,
    panic_count: AtomicUsize,
    stack_size: Option<usize>,
}

impl ThreadPoolSharedData {
    fn has_work(&self) -> bool {
        self.queued_count.load(Ordering::SeqCst) > 0 || self.active_count.load(Ordering::SeqCst) > 0
    }

    #[allow(unused_must_use)]
    fn no_work_notify_all(&self) {
        if !self.has_work() {
            self.empty_trigger
                .lock()
                .expect("Unable to notify all joining threads");
            self.empty_condvar.notify_all();
        }
    }
}

pub struct ThreadPool {
    jobs: Sender<Thunk<'static>>,
    shared_data: Arc<ThreadPoolSharedData>,
}

impl ThreadPool {
    pub fn new(num_threads: usize) -> ThreadPool {
        Builder::new().num_threads(num_threads).build()
    }

    pub fn with_name(name: String, num_threads: usize) -> ThreadPool {
        Builder::new()
            .num_threads(num_threads)
            .thread_name(name)
            .build()
    }

    pub fn execute<F>(&self, job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.shared_data.queued_count.fetch_add(1, Ordering::SeqCst);
        self.jobs
            .send(Box::new(job))
            .expect("ThreadPool::execute unable to send job into queue.");
    }

    pub fn queued_count(&self) -> usize {
        self.shared_data.queued_count.load(Ordering::Relaxed)
    }

    pub fn active_count(&self) -> usize {
        self.shared_data.active_count.load(Ordering::SeqCst)
    }

    pub fn max_count(&self) -> usize {
        self.shared_data.max_thread_count.load(Ordering::Relaxed)
    }

    pub fn panic_count(&self) -> usize {
        self.shared_data.panic_count.load(Ordering::Relaxed)
    }

    pub fn set_num_threads(&mut self, num_threads: usize) {
        assert!(num_threads >= 1);
        let prev_num_threads = self
            .shared_data
            .max_thread_count
            .swap(num_threads, Ordering::Release);
        if let Some(num_spawn) = num_threads.checked_sub(prev_num_threads) {
            // Spawn new threads
            for _ in 0..num_spawn {
                spawn_in_pool(self.shared_data.clone());
            }
        }
    }

    pub fn join(&self) {
        if !self.shared_data.has_work() {
            return;
        }

        let generation = self.shared_data.join_generation.load(Ordering::SeqCst);
        let mut lock = self.shared_data.empty_trigger.lock().unwrap();

        while generation == self.shared_data.join_generation.load(Ordering::Relaxed)
            && self.shared_data.has_work()
        {
            lock = self.shared_data.empty_condvar.wait(lock).unwrap();
        }

        // increase generation if we are the first thread to come out of the loop
        let _ = self.shared_data.join_generation.compare_exchange(
            generation,
            generation.wrapping_add(1),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

impl Clone for ThreadPool {
    fn clone(&self) -> ThreadPool {
        ThreadPool {
            jobs: self.jobs.clone(),
            shared_data: self.shared_data.clone(),
        }
    }
}

impl Default for ThreadPool {
    fn default() -> Self {
        ThreadPool::new(num_cpus::get())
    }
}

impl fmt::Debug for ThreadPool {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ThreadPool")
            .field("name", &self.shared_data.name)
            .field("queued_count", &self.queued_count())
            .field("active_count", &self.active_count())
            .field("max_count", &self.max_count())
            .finish()
    }
}

impl PartialEq for ThreadPool {
    fn eq(&self, other: &ThreadPool) -> bool {
        Arc::ptr_eq(&self.shared_data, &other.shared_data)
    }
}

fn spawn_in_pool(shared_data: Arc<ThreadPoolSharedData>) {
    let mut builder = thread::Builder::new();
    if let Some(ref name) = shared_data.name {
        builder = builder.name(name.clone());
    }
    if let Some(ref stack_size) = shared_data.stack_size {
        builder = builder.stack_size(stack_size.to_owned());
    }
    builder
        .spawn(move || {
            // Will spawn a new thread on panic unless it is cancelled.
            let sentinel = Sentinel::new(&shared_data);

            loop {
                // Shutdown this thread if the pool has become smaller
                let thread_counter_val = shared_data.active_count.load(Ordering::Acquire);
                let max_thread_count_val = shared_data.max_thread_count.load(Ordering::Relaxed);
                if thread_counter_val >= max_thread_count_val {
                    break;
                }
                let message = {
                    // Only lock jobs for the time it takes
                    // to get a job, not run it.
                    let lock = shared_data
                        .job_receiver
                        .lock()
                        .expect("Worker thread unable to lock job_receiver");
                    lock.recv()
                };

                let job = match message {
                    Ok(job) => job,
                    // The ThreadPool was dropped.
                    Err(..) => break,
                };
                // Do not allow IR around the job execution
                shared_data.active_count.fetch_add(1, Ordering::SeqCst);
                shared_data.queued_count.fetch_sub(1, Ordering::SeqCst);

                job.call_box();

                shared_data.active_count.fetch_sub(1, Ordering::SeqCst);
                shared_data.no_work_notify_all();
            }

            sentinel.cancel();
        })
        .unwrap();
}

#[cfg(test)]
mod test {
    use super::{Builder, ThreadPool};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{channel, sync_channel};
    use std::sync::{Arc, Barrier};
    use std::thread::{self, sleep};
    use std::time::Duration;

    const TEST_TASKS: usize = 4;

    #[ignore]
    #[test]
    fn test_set_num_threads_increasing() {
        let new_thread_amount = TEST_TASKS + 8;
        let mut pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..TEST_TASKS {
            pool.execute(move || sleep(Duration::from_secs(23)));
        }
        sleep(Duration::from_secs(1));
        assert_eq!(pool.active_count(), TEST_TASKS);

        pool.set_num_threads(new_thread_amount);

        for _ in 0..(new_thread_amount - TEST_TASKS) {
            pool.execute(move || sleep(Duration::from_secs(23)));
        }
        sleep(Duration::from_secs(1));
        assert_eq!(pool.active_count(), new_thread_amount);

        pool.join();
    }

    #[ignore]
    #[test]
    fn test_set_num_threads_decreasing() {
        let new_thread_amount = 2;
        let mut pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..TEST_TASKS {
            pool.execute(move || {
                assert_eq!(1, 1);
            });
        }
        pool.set_num_threads(new_thread_amount);
        for _ in 0..new_thread_amount {
            pool.execute(move || sleep(Duration::from_secs(23)));
        }
        sleep(Duration::from_secs(1));
        assert_eq!(pool.active_count(), new_thread_amount);

        pool.join();
    }

    #[ignore]
    #[test]
    fn test_active_count() {
        let pool = ThreadPool::new(TEST_TASKS);
        for _ in 0..2 * TEST_TASKS {
            pool.execute(move || loop {
                sleep(Duration::from_secs(10))
            });
        }
        sleep(Duration::from_secs(1));
        let active_count = pool.active_count();
        assert_eq!(active_count, TEST_TASKS);
        let initialized_count = pool.max_count();
        assert_eq!(initialized_count, TEST_TASKS);
    }

    #[ignore]
    #[test]
    fn test_works() {
        let pool = ThreadPool::new(TEST_TASKS);

        let (tx, rx) = channel();
        for _ in 0..TEST_TASKS {
            let tx = tx.clone();
            pool.execute(move || {
                tx.send(1).unwrap();
            });
        }

        let sum_tasks: usize = rx.iter().take(TEST_TASKS).sum();
        assert_eq!(sum_tasks, TEST_TASKS);
    }

    #[ignore]
    #[test]
    #[should_panic]
    fn test_zero_tasks_panic() {
        ThreadPool::new(0);
    }

    #[ignore]
    #[test]
    fn test_recovery_from_subtask_panic() {
        let pool = ThreadPool::new(TEST_TASKS);

        // Panic all the existing threads.
        for _ in 0..TEST_TASKS {
            pool.execute(move || panic!("Ignore this panic, it must!"));
        }
        pool.join();

        assert_eq!(pool.panic_count(), TEST_TASKS);

        // Ensure new threads were spawned to compensate.
        let (tx, rx) = channel();
        for _ in 0..TEST_TASKS {
            let tx = tx.clone();
            pool.execute(move || {
                tx.send(1).unwrap();
            });
        }

        let sum_tasks: usize = rx.iter().take(TEST_TASKS).sum();
        assert_eq!(sum_tasks, TEST_TASKS);
    }

    #[ignore]
    #[test]
    fn test_should_not_panic_on_drop_if_subtasks_panic_after_drop() {
        let pool = ThreadPool::new(TEST_TASKS);
        let waiter = Arc::new(Barrier::new(TEST_TASKS + 1));

        // Panic all the existing threads in a bit.
        for _ in 0..TEST_TASKS {
            let waiter = waiter.clone();
            pool.execute(move || {
                waiter.wait();
                panic!("Ignore this panic, it should!");
            });
        }

        drop(pool);

        // Kick off the failure.
        waiter.wait();
    }

    #[ignore]
    #[test]
    fn test_massive_task_creation() {
        let test_tasks = 4_200_000;

        let pool = ThreadPool::new(TEST_TASKS);
        let b0 = Arc::new(Barrier::new(TEST_TASKS + 1));
        let b1 = Arc::new(Barrier::new(TEST_TASKS + 1));

        let (tx, rx) = channel();

        for i in 0..test_tasks {
            let tx = tx.clone();
            let (b0, b1) = (b0.clone(), b1.clone());

            pool.execute(move || {
                // Wait until the pool has been filled once.
                if i < TEST_TASKS {
                    b0.wait();
                    // wait so the pool can be measured
                    b1.wait();
                }

                let _ = tx.send(1).is_ok();
            });
        }

        b0.wait();
        assert_eq!(pool.active_count(), TEST_TASKS);
        b1.wait();

        let sum_task: usize = rx.iter().take(test_tasks).sum();

        assert_eq!(sum_task, test_tasks);
        pool.join();

        let atomic_active_count = pool.active_count();
        assert_eq!(
            atomic_active_count, 0,
            "atomic_active_count: {}",
            atomic_active_count
        );
    }

    #[ignore]
    #[test]
    fn test_shrink() {
        let test_tasks_begin = TEST_TASKS + 2;

        let mut pool = ThreadPool::new(test_tasks_begin);
        let b0 = Arc::new(Barrier::new(test_tasks_begin + 1));
        let b1 = Arc::new(Barrier::new(test_tasks_begin + 1));

        for _ in 0..test_tasks_begin {
            let (b0, b1) = (b0.clone(), b1.clone());
            pool.execute(move || {
                b0.wait();
                b1.wait();
            });
        }

        let b2 = Arc::new(Barrier::new(TEST_TASKS + 1));
        let b3 = Arc::new(Barrier::new(TEST_TASKS + 1));

        for _ in 0..TEST_TASKS {
            let (b2, b3) = (b2.clone(), b3.clone());
            pool.execute(move || {
                b2.wait();
                b3.wait();
            });
        }

        b0.wait();
        pool.set_num_threads(TEST_TASKS);

        assert_eq!(pool.active_count(), test_tasks_begin);
        b1.wait();

        b2.wait();
        assert_eq!(pool.active_count(), TEST_TASKS);
        b3.wait();
    }

    #[ignore]
    #[test]
    fn test_name() {
        let name = "test";
        let mut pool = ThreadPool::with_name(name.to_owned(), 2);
        let (tx, rx) = sync_channel(0);

        // initial thread should share the name "test"
        for _ in 0..2 {
            let tx = tx.clone();
            pool.execute(move || {
                let name = thread::current().name().unwrap().to_owned();
                tx.send(name).unwrap();
            });
        }

        // new spawn thread should share the name "test" too.
        pool.set_num_threads(3);
        let tx_clone = tx.clone();
        pool.execute(move || {
            let name = thread::current().name().unwrap().to_owned();
            tx_clone.send(name).unwrap();
            panic!();
        });

        // recover thread should share the name "test" too.
        pool.execute(move || {
            let name = thread::current().name().unwrap().to_owned();
            tx.send(name).unwrap();
        });

        for thread_name in rx.iter().take(4) {
            assert_eq!(name, thread_name);
        }
    }

    #[ignore]
    #[test]
    fn test_debug() {
        let pool = ThreadPool::new(4);
        let debug = format!("{:?}", pool);
        assert_eq!(
            debug,
            "ThreadPool { name: None, queued_count: 0, active_count: 0, max_count: 4 }"
        );

        let pool = ThreadPool::with_name("hello".into(), 4);
        let debug = format!("{:?}", pool);
        assert_eq!(
            debug,
            "ThreadPool { name: Some(\"hello\"), queued_count: 0, active_count: 0, max_count: 4 }"
        );

        let pool = ThreadPool::new(4);
        pool.execute(move || sleep(Duration::from_secs(5)));
        sleep(Duration::from_secs(1));
        let debug = format!("{:?}", pool);
        assert_eq!(
            debug,
            "ThreadPool { name: None, queued_count: 0, active_count: 1, max_count: 4 }"
        );
    }

    #[ignore]
    #[test]
    fn test_repeate_join() {
        let pool = ThreadPool::with_name("repeat join test".into(), 8);
        let test_count = Arc::new(AtomicUsize::new(0));

        for _ in 0..42 {
            let test_count = test_count.clone();
            pool.execute(move || {
                sleep(Duration::from_secs(2));
                test_count.fetch_add(1, Ordering::Release);
            });
        }

        println!("{:?}", pool);
        pool.join();
        assert_eq!(42, test_count.load(Ordering::Acquire));

        for _ in 0..42 {
            let test_count = test_count.clone();
            pool.execute(move || {
                sleep(Duration::from_secs(2));
                test_count.fetch_add(1, Ordering::Relaxed);
            });
        }
        pool.join();
        assert_eq!(84, test_count.load(Ordering::Relaxed));
    }

    #[ignore]
    #[test]
    fn test_multi_join() {
        use std::sync::mpsc::TryRecvError::*;

        // Toggle the following lines to debug the deadlock
        fn error(_s: String) {
            //use ::std::io::Write;
            //let stderr = ::std::io::stderr();
            //let mut stderr = stderr.lock();
            //stderr.write(&_s.as_bytes()).is_ok();
        }

        let pool0 = ThreadPool::with_name("multi join pool0".into(), 4);
        let pool1 = ThreadPool::with_name("multi join pool1".into(), 4);
        let (tx, rx) = channel();

        for i in 0..8 {
            let pool1 = pool1.clone();
            let pool0_ = pool0.clone();
            let tx = tx.clone();
            pool0.execute(move || {
                pool1.execute(move || {
                    error(format!("p1: {} -=- {:?}\n", i, pool0_));
                    pool0_.join();
                    error(format!("p1: send({})\n", i));
                    tx.send(i).expect("send i from pool1 -> main");
                });
                error(format!("p0: {}\n", i));
            });
        }
        drop(tx);

        assert_eq!(rx.try_recv(), Err(Empty));
        error(format!("{:?}\n{:?}\n", pool0, pool1));
        pool0.join();
        error(format!("pool0.join() complete =-= {:?}", pool1));
        pool1.join();
        error("pool1.join() complete\n".into());
        let result: usize = rx.iter().sum();
        assert_eq!(result, 1 + 2 + 3 + 4 + 5 + 6 + 7);
    }

    #[ignore]
    #[test]
    fn test_empty_pool() {
        // Joining an empty pool must return imminently
        let pool = ThreadPool::new(4);

        pool.join();
    }

    #[ignore]
    #[test]
    fn test_no_fun_or_joy() {
        // What happens when you keep adding jobs after a join

        fn sleepy_function() {
            sleep(Duration::from_secs(6));
        }

        let pool = ThreadPool::with_name("no fun or joy".into(), 8);

        pool.execute(sleepy_function);

        let p_t = pool.clone();
        thread::spawn(move || {
            (0..23).map(|_| p_t.execute(sleepy_function)).count();
        });

        pool.join();
    }

    #[ignore]
    #[test]
    fn test_clone() {
        let pool = ThreadPool::with_name("clone example".into(), 2);

        // This batch of jobs will occupy the pool for some time
        for _ in 0..6 {
            pool.execute(move || {
                sleep(Duration::from_secs(2));
            });
        }

        // The following jobs will be inserted into the pool in a random fashion
        let t0 = {
            let pool = pool.clone();
            thread::spawn(move || {
                // wait for the first batch of tasks to finish
                pool.join();

                let (tx, rx) = channel();
                for i in 0..42 {
                    let tx = tx.clone();
                    pool.execute(move || {
                        tx.send(i).expect("channel will be waiting");
                    });
                }
                drop(tx);
                let result: usize = rx.iter().sum();
                result
            })
        };
        let t1 = {
            let pool = pool.clone();
            thread::spawn(move || {
                // wait for the first batch of tasks to finish
                pool.join();

                let (tx, rx) = channel();
                for i in 1..12 {
                    let tx = tx.clone();
                    pool.execute(move || {
                        tx.send(i).expect("channel will be waiting");
                    });
                }
                drop(tx);
                let result: usize = rx.iter().product();
                result
            })
        };

        assert_eq!(
            861,
            t0.join()
                .expect("thread 0 will return after calculating additions",)
        );
        assert_eq!(
            39916800,
            t1.join()
                .expect("thread 1 will return after calculating multiplications",)
        );
    }

    #[ignore]
    #[test]
    fn test_sync_shared_data() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<super::ThreadPoolSharedData>();
    }

    #[ignore]
    #[test]
    fn test_send_shared_data() {
        fn assert_send<T: Send>() {}
        assert_send::<super::ThreadPoolSharedData>();
    }

    #[ignore]
    #[test]
    fn test_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ThreadPool>();
    }

    #[ignore]
    #[test]
    fn test_cloned_eq() {
        let a = ThreadPool::new(2);

        assert_eq!(a, a.clone());
    }

    #[ignore]
    #[test]
    /// The scenario is joining threads should not be stuck once their wave
    /// of joins has completed. So once one thread joining on a pool has
    /// succeeded other threads joining on the same pool must get out even if
    /// the thread is used for other jobs while the first group is finishing
    /// their join
    ///
    /// In this example this means the waiting threads will exit the join in
    /// groups of four because the waiter pool has four workers.
    fn test_join_wavesurfer() {
        let n_cycles = 4;
        let n_workers = 4;
        let (tx, rx) = channel();
        let builder = Builder::new()
            .num_threads(n_workers)
            .thread_name("join wavesurfer".into());
        let p_waiter = builder.clone().build();
        let p_clock = builder.build();

        let barrier = Arc::new(Barrier::new(3));
        let wave_clock = Arc::new(AtomicUsize::new(0));
        let clock_thread = {
            let barrier = barrier.clone();
            let wave_clock = wave_clock.clone();
            thread::spawn(move || {
                barrier.wait();
                for wave_num in 0..n_cycles {
                    wave_clock.store(wave_num, Ordering::SeqCst);
                    sleep(Duration::from_secs(1));
                }
            })
        };

        {
            let barrier = barrier.clone();
            p_clock.execute(move || {
                barrier.wait();
                // this sleep is for stabilisation on weaker platforms
                sleep(Duration::from_millis(100));
            });
        }

        // prepare three waves of jobs
        for i in 0..3 * n_workers {
            let p_clock = p_clock.clone();
            let tx = tx.clone();
            let wave_clock = wave_clock.clone();
            p_waiter.execute(move || {
                let now = wave_clock.load(Ordering::SeqCst);
                p_clock.join();
                // submit jobs for the second wave
                p_clock.execute(|| sleep(Duration::from_secs(1)));
                let clock = wave_clock.load(Ordering::SeqCst);
                tx.send((now, clock, i)).unwrap();
            });
        }
        println!("all scheduled at {}", wave_clock.load(Ordering::SeqCst));
        barrier.wait();

        p_clock.join();
        //p_waiter.join();

        drop(tx);
        let mut hist = vec![0; n_cycles];
        let mut data = vec![];
        for (now, after, i) in rx.iter() {
            let mut dur = after - now;
            if dur >= n_cycles - 1 {
                dur = n_cycles - 1;
            }
            hist[dur] += 1;

            data.push((now, after, i));
        }
        for (i, n) in hist.iter().enumerate() {
            println!(
                "\t{}: {} {}",
                i,
                n,
                &*(0..*n).fold("".to_owned(), |s, _| s + "*")
            );
        }
        assert!(data.iter().all(|&(cycle, stop, i)| if i < n_workers {
            cycle == stop
        } else {
            cycle < stop
        }));

        clock_thread.join().unwrap();
    }
}
