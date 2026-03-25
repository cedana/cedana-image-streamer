use std::sync::{Condvar, Mutex};
use std::cmp::min;

pub struct Semaphore {
    lock: Mutex<isize>,
    cvar: Condvar,
    start_val: isize
}

impl Semaphore {
    pub fn new(count: isize) -> Semaphore {
        Semaphore {
            lock: Mutex::new(count),
            cvar: Condvar::new(),
            start_val: count
        }
    }

    pub fn acquire(&self, want: isize) {
        let mut count = self.lock.lock().unwrap();
        while *count < want {
            count = self.cvar.wait(count).unwrap();
        }
        *count -= want;
    }

    pub fn release(&self, to_realease: isize) {
        let mut count = self.lock.lock().unwrap();
        *count = min(*count + to_realease, self.start_val);
        self.cvar.notify_all();
    }
}
