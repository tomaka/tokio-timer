//! This code is needed in order to support a channel that can receive with a
//! timeout.

use Builder;
use mpmc::Queue;
use wheel::{Token, Wheel};
use futures::task::Task;
use std::sync::Arc;
#[cfg(target_os = "emscripten")]
use std::sync::Mutex;
#[cfg(not(target_os = "emscripten"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
#[cfg(not(target_os = "emscripten"))]
use std::thread::{self, Thread};
#[cfg(target_os = "emscripten")]
use stdweb;

#[derive(Clone)]
pub struct Worker {
    tx: Arc<Tx>,
}

/// Communicate with the timer thread
struct Tx {
    chan: Arc<Chan>,
    #[cfg(not(target_os = "emscripten"))]
    worker: Thread,
    tolerance: Duration,
    max_timeout: Duration,
}

struct Chan {
    #[cfg(not(target_os = "emscripten"))]
    run: AtomicBool,
    #[cfg(target_os = "emscripten")]
    interval_id: Mutex<u32>,
    set_timeouts: SetQueue,
    mod_timeouts: ModQueue,
}

/// Messages sent on the `set_timeouts` exchange
struct SetTimeout(Instant, Task);

/// Messages sent on the `mod_timeouts` queue
enum ModTimeout {
    Move(Token, Instant, Task),
    Cancel(Token, Instant),
}

type SetQueue = Queue<SetTimeout, Token>;
type ModQueue = Queue<ModTimeout, ()>;

impl Worker {
    /// Spawn a worker, returning a handle to allow communication
    pub fn spawn(wheel: Wheel, builder: Builder) -> Worker {
        Worker::spawn_inner(wheel, builder)
    }

    #[cfg(not(target_os = "emscripten"))]
    fn spawn_inner(mut wheel: Wheel, builder: Builder) -> Worker {
        let tolerance = builder.get_tick_duration();
        let max_timeout = builder.get_max_timeout();
        let capacity = builder.get_channel_capacity();

        // Assert that the wheel has at least capacity available timeouts
        assert!(wheel.available() >= capacity);

        let chan = Arc::new(Chan {
            run: AtomicBool::new(true),
            set_timeouts: Queue::with_capacity(capacity, || wheel.reserve().unwrap()),
            mod_timeouts: Queue::with_capacity(capacity, || ()),
        });

        let chan2 = chan.clone();

        // Spawn the worker thread
        let t = thread::Builder::new()
            .name(builder.thread_name.unwrap_or_else(|| "tokio-timer".to_owned()))
            .spawn(move || run(chan2, wheel))
            .expect("thread::spawn");

        Worker {
            tx: Arc::new(Tx {
                chan: chan,
                worker: t.thread().clone(),
                tolerance: tolerance,
                max_timeout: max_timeout,
            }),
        }
    }

    #[cfg(target_os = "emscripten")]
    fn spawn_inner(mut wheel: Wheel, builder: Builder) -> Worker {
        stdweb::initialize();

        let tolerance = builder.get_tick_duration();
        let max_timeout = builder.get_max_timeout();
        let capacity = builder.get_channel_capacity();

        // Assert that the wheel has at least capacity available timeouts
        assert!(wheel.available() >= capacity);

        let chan = Arc::new(Chan {
            interval_id: Mutex::new(0),
            set_timeouts: Queue::with_capacity(capacity, || wheel.reserve().unwrap()),
            mod_timeouts: Queue::with_capacity(capacity, || ()),
        });

        let chan2 = chan.clone();

        *chan.interval_id.lock().unwrap() = {
            use stdweb::unstable::TryInto;
            let interval = 1000;//tolerance.as_secs() as u32 * 1000 + tolerance.subsec_nanos() / 1000000;
            let cb = move || {
                run_once(&chan2, &mut wheel);
            };
            let res = js!{
                var cb = @{cb};
                return setInterval(function() {
                    cb();
                }, @{interval});
            };
            res.try_into().unwrap()
        };

        Worker {
            tx: Arc::new(Tx {
                chan: chan,
                tolerance: tolerance,
                max_timeout: max_timeout,
            }),
        }
    }

    /// The earliest a timeout can fire before the requested `Instance`
    pub fn tolerance(&self) -> &Duration {
        &self.tx.tolerance
    }

    pub fn max_timeout(&self) -> &Duration {
        &self.tx.max_timeout
    }

    /// Set a timeout
    pub fn set_timeout(&self, when: Instant, task: Task) -> Result<Token, Task> {
        self.set_timeout_inner(when, task)
    }

    #[cfg(not(target_os = "emscripten"))]
    fn set_timeout_inner(&self, when: Instant, task: Task) -> Result<Token, Task> {
        self.tx.chan.set_timeouts.push(SetTimeout(when, task))
            .and_then(|ret| {
                // Unpark the timer thread
                self.tx.worker.unpark();
                Ok(ret)
            })
            .map_err(|SetTimeout(_, task)| task)
    }

    #[cfg(target_os = "emscripten")]
    fn set_timeout_inner(&self, when: Instant, task: Task) -> Result<Token, Task> {
        self.tx.chan.set_timeouts.push(SetTimeout(when, task))
            .map_err(|SetTimeout(_, task)| task)
    }

    /// Move a timeout
    pub fn move_timeout(&self, token: Token, when: Instant, task: Task) -> Result<(), Task> {
        self.move_timeout_inner(token, when, task)
    }

    #[cfg(not(target_os = "emscripten"))]
    fn move_timeout_inner(&self, token: Token, when: Instant, task: Task) -> Result<(), Task> {
        self.tx.chan.mod_timeouts.push(ModTimeout::Move(token, when, task))
            .and_then(|ret| {
                self.tx.worker.unpark();
                Ok(ret)
            })
            .map_err(|v| {
                match v {
                    ModTimeout::Move(_, _, task) => task,
                    _ => unreachable!(),
                }
            })
    }

    #[cfg(target_os = "emscripten")]
    fn move_timeout_inner(&self, token: Token, when: Instant, task: Task) -> Result<(), Task> {
        self.tx.chan.mod_timeouts.push(ModTimeout::Move(token, when, task))
            .map_err(|v| {
                match v {
                    ModTimeout::Move(_, _, task) => task,
                    _ => unreachable!(),
                }
            })
    }

    /// Cancel a timeout
    pub fn cancel_timeout(&self, token: Token, instant: Instant) {
        // The result here is ignored because:
        //
        // 1) this fn is only called when the timeout is dropping, so nothing
        //    can be done with the result
        // 2) Not being able to cancel a timeout is not a huge deal and only
        //    results in a spurious wakeup.
        //
        let _ = self.tx.chan.mod_timeouts.push(ModTimeout::Cancel(token, instant));
    }
}

#[cfg(not(target_os = "emscripten"))]
fn run(chan: Arc<Chan>, mut wheel: Wheel) {
    while chan.run.load(Ordering::Relaxed) {
        run_once(&chan, &mut wheel);

        let now = Instant::now();

        if let Some(next) = wheel.next_timeout() {
            if next > now {
                thread::park_timeout(next - now);
            }
        } else {
            thread::park();
        }
    }
}

fn run_once(chan: &Chan, wheel: &mut Wheel) {
    let now = Instant::now();

    // Fire off all expired timeouts
    while let Some(task) = wheel.poll(now) {
        task.notify();
    }

    // As long as the wheel has capacity to manage new timeouts, read off
    // of the queue.
    while let Some(token) = wheel.reserve() {
        match chan.set_timeouts.pop(token) {
            Ok((SetTimeout(when, task), token)) => {
                wheel.set_timeout(token, when, task);
            }
            Err(token) => {
                wheel.release(token);
                break;
            }
        }
    }

    loop {
        match chan.mod_timeouts.pop(()) {
            Ok((ModTimeout::Move(token, when, task), _)) => {
                wheel.move_timeout(token, when, task);
            }
            Ok((ModTimeout::Cancel(token, when), _)) => {
                wheel.cancel(token, when);
            }
            Err(_) => break,
        }
    }
}

#[cfg(not(target_os = "emscripten"))]
impl Drop for Tx {
    fn drop(&mut self) {
        self.chan.run.store(false, Ordering::Relaxed);
        self.worker.unpark();
    }
}

#[cfg(target_os = "emscripten")]
impl Drop for Tx {
    fn drop(&mut self) {
        let id = *self.chan.interval_id.lock().unwrap();
        js!{ clearInterval(@{id}); }
    }
}
