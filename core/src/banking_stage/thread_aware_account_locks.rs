use {
    solana_sdk::pubkey::Pubkey,
    std::{
        collections::{hash_map::Entry, HashMap},
        fmt::{Debug, Display},
        ops::{BitAndAssign, Sub},
    },
};

pub(crate) const MAX_THREADS: usize = u64::BITS as usize;

/// Identifier for a thread
pub(crate) type ThreadId = usize; // 0..MAX_THREADS-1

type LockCount = u32;

/// A bit-set of threads an account is scheduled or can be scheduled for.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct ThreadSet(u64);

/// Thread-aware account locks which allows for scheduling on threads
/// that already hold locks on the account. This is useful for allowing
/// queued transactions to be scheduled on a thread while the transaction
/// is still being executed on the thread, up to a queue limit.
pub(crate) struct ThreadAwareAccountLocks {
    /// Number of threads.
    num_threads: usize, // 0..MAX_THREADS
    /// Limit on the number of sequentially-queued transactions per account.
    sequential_queue_limit: u32,
    /// Write locks - only on thread can hold a write lock at a time.
    /// Contains how many write locks are held by the thread.
    write_locks: HashMap<Pubkey, (ThreadId, u32)>,
    /// Read locks - multiple threads can hold a read lock at a time.
    /// Contains thread-set for easily checking which threads are scheduled.
    /// Contains how many read locks are held by each thread.
    read_locks: HashMap<Pubkey, (ThreadSet, [LockCount; MAX_THREADS])>,
}

impl ThreadAwareAccountLocks {
    /// Creates a new `ThreadAwareAccountLocks` with the given number of threads
    /// and queue limit.
    pub(crate) fn new(num_threads: usize, sequential_queue_limit: u32) -> Self {
        assert!(num_threads > 0, "num threads must be > 0");
        assert!(
            num_threads <= MAX_THREADS,
            "num threads must be <= {MAX_THREADS}"
        );
        assert!(
            sequential_queue_limit > 0,
            "sequential queue limit must be > 0"
        );

        Self {
            num_threads,
            sequential_queue_limit,
            write_locks: HashMap::new(),
            read_locks: HashMap::new(),
        }
    }

    /// Returns the `ThreadId` if the accounts are able to be locked
    /// for the given thread, otherwise `None` is returned.
    /// If accounts are schedulable, then they are locked for the thread
    /// selected by the `thread_selector` function.
    pub(crate) fn try_lock_accounts<'a>(
        &mut self,
        write_account_locks: impl Iterator<Item = &'a Pubkey> + Clone,
        read_account_locks: impl Iterator<Item = &'a Pubkey> + Clone,
        thread_selector: impl FnOnce(ThreadSet) -> ThreadId,
    ) -> Option<ThreadId> {
        let schedulable_threads = self
            .accounts_schedulable_threads(write_account_locks.clone(), read_account_locks.clone());
        (!schedulable_threads.is_empty()).then(|| {
            let thread_id = thread_selector(schedulable_threads);
            self.lock_accounts(write_account_locks, read_account_locks, thread_id);
            thread_id
        })
    }

    /// Returns `ThreadSet` that the given accounts can be scheduled on.
    fn accounts_schedulable_threads<'a>(
        &self,
        write_account_locks: impl Iterator<Item = &'a Pubkey>,
        read_account_locks: impl Iterator<Item = &'a Pubkey>,
    ) -> ThreadSet {
        let mut schedulable_threads = ThreadSet::any(self.num_threads);

        for account in write_account_locks {
            schedulable_threads &= self.write_schedulable_threads(account);
        }

        for account in read_account_locks {
            schedulable_threads &= self.read_schedulable_threads(account);
        }

        schedulable_threads
    }

    /// Returns `ThreadSet` of schedulable threads for the given readable account.
    /// If the account is not locked, then all threads are schedulable.
    /// If only read locked, then all threads are schedulable.
    /// If write-locked, then only the thread holding the write lock is schedulable.
    /// The sequential limit is checked, and a thread will not be returned as schedulable
    /// if the limit is reached.
    fn read_schedulable_threads(&self, account: &Pubkey) -> ThreadSet {
        // If the account is only read locked, then a read lock can be taken on any thread
        // that is not at the sequential limit.
        self.schedulable_threads_with_read_only_handler(account, |thread_set, counts| {
            let mut schedulable_threads = ThreadSet::any(self.num_threads);
            for thread_id in thread_set.threads_iter() {
                if counts[thread_id] == self.sequential_queue_limit {
                    schedulable_threads.remove(thread_id);
                }
            }
            schedulable_threads
        })
    }

    /// Returns `ThreadSet` of schedulable threads for the given writable account.
    /// If the account is not locked, then all threads are schedulable.
    /// If read-locked on a single thread, then only that thread is schedulable.
    /// If write-locked, then only that thread is schedulable.
    /// In all other cases, no threads are schedulable.
    /// The sequential limit is checked, and a thread will not be returned as schedulable
    /// if the limit is reached.
    fn write_schedulable_threads(&self, account: &Pubkey) -> ThreadSet {
        // If the account is only read locked, then a write lock can only be taken
        // if the read lock is held by a single thread, and the limit is not exceeded.
        self.schedulable_threads_with_read_only_handler(account, |thread_set, counts| {
            thread_set
                .only_one_contained()
                .filter(|thread_id| counts[*thread_id] < self.sequential_queue_limit)
                .map_or_else(ThreadSet::none, ThreadSet::only)
        })
    }

    /// Returns `ThreadSet` of schedulable threads, given the read-only lock handler.
    /// Helper function, since the only difference between read and write schedulable threads
    /// is in how the case where only read locks are held is handled.
    /// If there are no locks, then all threads are schedulable.
    /// If only write-locked, then only the thread holding the write lock is schedulable.
    /// If a mix of locks, then only the write thread is schedulable.
    /// The sequential limit is checked, and a thread will not be returned as schedulable
    /// if the limit is reached.
    fn schedulable_threads_with_read_only_handler(
        &self,
        account: &Pubkey,
        read_only_handler: impl Fn(&ThreadSet, &[u32]) -> ThreadSet,
    ) -> ThreadSet {
        match (self.write_locks.get(account), self.read_locks.get(account)) {
            (None, None) => ThreadSet::any(self.num_threads),
            (None, Some((thread_set, counts))) => read_only_handler(thread_set, counts),
            (Some((thread_id, count)), None) => {
                if count == &self.sequential_queue_limit {
                    ThreadSet::none()
                } else {
                    ThreadSet::only(*thread_id)
                }
            }
            (Some((thread_id, count)), Some((thread_set, counts))) => {
                assert_eq!(Some(*thread_id), thread_set.only_one_contained());
                if count + counts[*thread_id] == self.sequential_queue_limit {
                    ThreadSet::none()
                } else {
                    ThreadSet::only(*thread_id)
                }
            }
        }
    }

    /// Add locks for all writable and readable accounts on `thread_id`.
    pub(crate) fn lock_accounts<'a>(
        &mut self,
        write_account_locks: impl Iterator<Item = &'a Pubkey>,
        read_account_locks: impl Iterator<Item = &'a Pubkey>,
        thread_id: ThreadId,
    ) {
        assert!(
            thread_id < self.num_threads,
            "thread_id must be < num_threads"
        );
        for account in write_account_locks {
            self.write_lock_account(account, thread_id);
        }

        for account in read_account_locks {
            self.read_lock_account(account, thread_id);
        }
    }

    /// Locks the given `account` for writing on `thread_id`.
    /// Panics if the account is already locked for writing on another thread.
    fn write_lock_account(&mut self, account: &Pubkey, thread_id: ThreadId) {
        match self.write_locks.entry(*account) {
            Entry::Occupied(mut entry) => {
                let (lock_thread_id, lock_count) = entry.get_mut();
                assert_eq!(
                    *lock_thread_id, thread_id,
                    "outstanding write lock must be on same thread"
                );

                *lock_count += 1;
                assert!(
                    *lock_count <= self.sequential_queue_limit,
                    "sequential queue limit must not be exceeded"
                );
            }
            Entry::Vacant(entry) => {
                entry.insert((thread_id, 1));
            }
        }

        // Check for outstanding read-locks
        if let Some((read_thread_set, _)) = self.read_locks.get(account) {
            assert_eq!(
                read_thread_set,
                &ThreadSet::only(thread_id),
                "outstanding read lock must be on same thread"
            );
        }
    }

    /// Unlocks the given `account` for writing on `thread_id`.
    /// Panics if the account is not locked for writing on `thread_id`.
    fn write_unlock_account(&mut self, account: &Pubkey, thread_id: ThreadId) {
        match self.write_locks.entry(*account) {
            Entry::Occupied(mut entry) => {
                let (lock_thread_id, lock_count) = entry.get_mut();
                assert_eq!(
                    *lock_thread_id, thread_id,
                    "outstanding write lock must be on same thread"
                );
                *lock_count -= 1;
                if *lock_count == 0 {
                    entry.remove();
                }
            }
            Entry::Vacant(_) => {
                panic!("write lock must exist for account: {account}");
            }
        }
    }

    /// Locks the given `account` for reading on `thread_id`.
    /// Panics if the account is already locked for writing on another thread.
    fn read_lock_account(&mut self, account: &Pubkey, thread_id: ThreadId) {
        match self.read_locks.entry(*account) {
            Entry::Occupied(mut entry) => {
                let (thread_set, lock_counts) = entry.get_mut();
                thread_set.insert(thread_id);
                lock_counts[thread_id] += 1;
            }
            Entry::Vacant(entry) => {
                let mut lock_counts = [0; MAX_THREADS];
                lock_counts[thread_id] = 1;
                entry.insert((ThreadSet::only(thread_id), lock_counts));
            }
        }

        // Check for outstanding write-locks
        if let Some((write_thread_id, _)) = self.write_locks.get(account) {
            assert_eq!(
                write_thread_id, &thread_id,
                "outstanding write lock must be on same thread"
            );
        }
    }

    /// Unlocks the given `account` for reading on `thread_id`.
    /// Panics if the account is not locked for reading on `thread_id`.
    fn read_unlock_account(&mut self, account: &Pubkey, thread_id: ThreadId) {
        match self.read_locks.entry(*account) {
            Entry::Occupied(mut entry) => {
                let (thread_set, lock_counts) = entry.get_mut();
                assert!(
                    thread_set.contains(thread_id),
                    "outstanding read lock must be on same thread"
                );
                lock_counts[thread_id] -= 1;
                if lock_counts[thread_id] == 0 {
                    thread_set.remove(thread_id);
                    if thread_set.is_empty() {
                        entry.remove();
                    }
                }
            }
            Entry::Vacant(_) => {
                panic!("read lock must exist for account: {account}");
            }
        }
    }
}

impl BitAndAssign for ThreadSet {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl Sub for ThreadSet {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 & !rhs.0)
    }
}

impl Display for ThreadSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ThreadSet({:#0width$b})", self.0, width = MAX_THREADS)
    }
}

impl Debug for ThreadSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl ThreadSet {
    #[inline(always)]
    pub(crate) fn none() -> Self {
        Self(0)
    }

    #[inline(always)]
    pub(crate) fn any(num_threads: usize) -> Self {
        Self(Self::as_flag(num_threads) - 1)
    }

    #[inline(always)]
    pub(crate) fn only(thread_id: ThreadId) -> Self {
        Self(Self::as_flag(thread_id))
    }

    #[inline(always)]
    pub(crate) fn num_threads(&self) -> u8 {
        self.0.count_ones() as u8
    }

    #[inline(always)]
    pub(crate) fn only_one_contained(&self) -> Option<ThreadId> {
        (self.num_threads() == 1).then_some(self.0.trailing_zeros() as ThreadId)
    }

    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self == &Self::none()
    }

    #[inline(always)]
    pub(crate) fn contains(&self, thread_id: ThreadId) -> bool {
        self.0 & (Self::as_flag(thread_id)) != 0
    }

    #[inline(always)]
    pub(crate) fn insert(&mut self, thread_id: ThreadId) {
        self.0 |= Self::as_flag(thread_id);
    }

    #[inline(always)]
    pub(crate) fn remove(&mut self, thread_id: ThreadId) {
        self.0 &= !Self::as_flag(thread_id);
    }

    #[inline(always)]
    pub(crate) fn threads_iter(self) -> impl Iterator<Item = ThreadId> {
        (0..MAX_THREADS).filter(move |thread_id| self.contains(*thread_id))
    }

    #[inline(always)]
    fn as_flag(thread_id: ThreadId) -> u64 {
        1 << thread_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_NUM_THREADS: usize = 4;
    const TEST_SEQ_LIMIT: u32 = 2;

    // Simple thread selector to select the first schedulable thread
    fn test_thread_selector(thread_set: ThreadSet) -> ThreadId {
        thread_set.threads_iter().next().unwrap()
    }

    #[test]
    #[should_panic(expected = "num threads must be > 0")]
    fn test_too_few_num_threads() {
        ThreadAwareAccountLocks::new(0, TEST_SEQ_LIMIT);
    }

    #[test]
    #[should_panic(expected = "num threads must be <=")]
    fn test_too_many_num_threads() {
        ThreadAwareAccountLocks::new(MAX_THREADS + 1, TEST_SEQ_LIMIT);
    }

    #[test]
    #[should_panic(expected = "sequential queue limit must be > 0")]
    fn test_invalid_sequential_queue_limit() {
        ThreadAwareAccountLocks::new(TEST_NUM_THREADS, 0);
    }

    #[test]
    fn test_try_lock_accounts_none() {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.read_lock_account(&pk1, 2);
        locks.read_lock_account(&pk1, 3);
        assert_eq!(
            locks.try_lock_accounts([&pk1].into_iter(), [&pk2].into_iter(), test_thread_selector),
            None
        );
    }

    #[test]
    fn test_try_lock_accounts_one() {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.write_lock_account(&pk2, 3);

        assert_eq!(
            locks.try_lock_accounts([&pk1].into_iter(), [&pk2].into_iter(), test_thread_selector),
            Some(3)
        );
    }

    #[test]
    fn test_try_lock_accounts_multiple() {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.read_lock_account(&pk2, 0);
        locks.read_lock_account(&pk2, 0);

        assert_eq!(
            locks.try_lock_accounts([&pk1].into_iter(), [&pk2].into_iter(), test_thread_selector),
            Some(1)
        );
    }

    #[test]
    fn test_try_lock_accounts_any() {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        assert_eq!(
            locks.try_lock_accounts([&pk1].into_iter(), [&pk2].into_iter(), test_thread_selector),
            Some(0)
        );
    }

    #[test]
    fn test_accounts_schedulable_threads_no_outstanding_locks() {
        let pk1 = Pubkey::new_unique();
        let locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);

        assert_eq!(
            locks.accounts_schedulable_threads([&pk1].into_iter(), std::iter::empty()),
            ThreadSet::any(TEST_NUM_THREADS)
        );
        assert_eq!(
            locks.accounts_schedulable_threads(std::iter::empty(), [&pk1].into_iter()),
            ThreadSet::any(TEST_NUM_THREADS)
        );
    }

    #[test]
    fn test_accounts_schedulable_threads_outstanding_write_only() {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);

        locks.write_lock_account(&pk1, 2);
        assert_eq!(
            locks.accounts_schedulable_threads([&pk1, &pk2].into_iter(), std::iter::empty()),
            ThreadSet::only(2)
        );
        assert_eq!(
            locks.accounts_schedulable_threads(std::iter::empty(), [&pk1, &pk2].into_iter()),
            ThreadSet::only(2)
        );

        locks.write_lock_account(&pk1, 2); // at limit
        assert_eq!(
            locks.accounts_schedulable_threads([&pk1, &pk2].into_iter(), std::iter::empty()),
            ThreadSet::none()
        );
        assert_eq!(
            locks.accounts_schedulable_threads(std::iter::empty(), [&pk1, &pk2].into_iter()),
            ThreadSet::none()
        );
    }

    #[test]
    fn test_accounts_schedulable_threads_outstanding_read_only() {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);

        locks.read_lock_account(&pk1, 2);
        assert_eq!(
            locks.accounts_schedulable_threads([&pk1, &pk2].into_iter(), std::iter::empty()),
            ThreadSet::only(2)
        );
        assert_eq!(
            locks.accounts_schedulable_threads(std::iter::empty(), [&pk1, &pk2].into_iter()),
            ThreadSet::any(TEST_NUM_THREADS)
        );

        locks.read_lock_account(&pk1, 0);
        assert_eq!(
            locks.accounts_schedulable_threads([&pk1, &pk2].into_iter(), std::iter::empty()),
            ThreadSet::none()
        );
        assert_eq!(
            locks.accounts_schedulable_threads(std::iter::empty(), [&pk1, &pk2].into_iter()),
            ThreadSet::any(TEST_NUM_THREADS)
        );

        locks.read_lock_account(&pk1, 0); // at limit
        assert_eq!(
            locks.accounts_schedulable_threads(std::iter::empty(), [&pk1, &pk2].into_iter()),
            ThreadSet::any(TEST_NUM_THREADS) - ThreadSet::only(0)
        );
    }

    #[test]
    fn test_accounts_schedulable_threads_outstanding_mixed() {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, 3);

        locks.read_lock_account(&pk1, 2);
        locks.write_lock_account(&pk1, 2);
        assert_eq!(
            locks.accounts_schedulable_threads([&pk1, &pk2].into_iter(), std::iter::empty()),
            ThreadSet::only(2)
        );
        assert_eq!(
            locks.accounts_schedulable_threads(std::iter::empty(), [&pk1, &pk2].into_iter()),
            ThreadSet::only(2)
        );

        locks.read_lock_account(&pk1, 2); // at limit
        assert_eq!(
            locks.accounts_schedulable_threads([&pk1, &pk2].into_iter(), std::iter::empty()),
            ThreadSet::none()
        );
        assert_eq!(
            locks.accounts_schedulable_threads(std::iter::empty(), [&pk1, &pk2].into_iter()),
            ThreadSet::none()
        );
    }

    #[test]
    #[should_panic(expected = "outstanding write lock must be on same thread")]
    fn test_write_lock_account_write_conflict_panic() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.write_lock_account(&pk1, 0);
        locks.write_lock_account(&pk1, 1);
    }

    #[test]
    #[should_panic(expected = "outstanding read lock must be on same thread")]
    fn test_write_lock_account_read_conflict_panic() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.read_lock_account(&pk1, 0);
        locks.write_lock_account(&pk1, 1);
    }

    #[test]
    #[should_panic(expected = "sequential queue limit must not be exceeded")]
    fn test_write_lock_account_limit() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.write_lock_account(&pk1, 1);
        locks.write_lock_account(&pk1, 1);
        locks.write_lock_account(&pk1, 1);
    }

    #[test]
    #[should_panic(expected = "write lock must exist")]
    fn test_write_unlock_account_not_locked() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.write_unlock_account(&pk1, 0);
    }

    #[test]
    #[should_panic(expected = "outstanding write lock must be on same thread")]
    fn test_write_unlock_account_thread_mismatch() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.write_lock_account(&pk1, 1);
        locks.write_unlock_account(&pk1, 0);
    }

    #[test]
    #[should_panic(expected = "outstanding write lock must be on same thread")]
    fn test_read_lock_account_write_conflict_panic() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.write_lock_account(&pk1, 0);
        locks.read_lock_account(&pk1, 1);
    }

    #[test]
    #[should_panic(expected = "read lock must exist")]
    fn test_read_unlock_account_not_locked() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.read_unlock_account(&pk1, 1);
    }

    #[test]
    #[should_panic(expected = "outstanding read lock must be on same thread")]
    fn test_read_unlock_account_thread_mismatch() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.read_lock_account(&pk1, 0);
        locks.read_unlock_account(&pk1, 1);
    }

    #[test]
    fn test_write_locking() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.write_lock_account(&pk1, 1);
        locks.write_lock_account(&pk1, 1);
        locks.write_unlock_account(&pk1, 1);
        locks.write_unlock_account(&pk1, 1);
        assert!(locks.write_locks.is_empty());
    }

    #[test]
    fn test_read_locking() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.read_lock_account(&pk1, 1);
        locks.read_lock_account(&pk1, 1);
        locks.read_unlock_account(&pk1, 1);
        locks.read_unlock_account(&pk1, 1);
        assert!(locks.read_locks.is_empty());
    }

    #[test]
    #[should_panic(expected = "thread_id must be < num_threads")]
    fn test_lock_accounts_invalid_thread() {
        let pk1 = Pubkey::new_unique();
        let mut locks = ThreadAwareAccountLocks::new(TEST_NUM_THREADS, TEST_SEQ_LIMIT);
        locks.lock_accounts([&pk1].into_iter(), std::iter::empty(), TEST_NUM_THREADS);
    }

    #[test]
    fn test_thread_set() {
        let mut thread_set = ThreadSet::none();
        assert!(thread_set.is_empty());
        assert_eq!(thread_set.num_threads(), 0);
        assert_eq!(thread_set.only_one_contained(), None);
        for idx in 0..MAX_THREADS {
            assert!(!thread_set.contains(idx));
        }

        thread_set.insert(4);
        assert!(!thread_set.is_empty());
        assert_eq!(thread_set.num_threads(), 1);
        assert_eq!(thread_set.only_one_contained(), Some(4));
        for idx in 0..MAX_THREADS {
            assert_eq!(thread_set.contains(idx), idx == 4);
        }

        thread_set.insert(2);
        assert!(!thread_set.is_empty());
        assert_eq!(thread_set.num_threads(), 2);
        assert_eq!(thread_set.only_one_contained(), None);
        for idx in 0..MAX_THREADS {
            assert_eq!(thread_set.contains(idx), idx == 2 || idx == 4);
        }

        thread_set.remove(4);
        assert!(!thread_set.is_empty());
        assert_eq!(thread_set.num_threads(), 1);
        assert_eq!(thread_set.only_one_contained(), Some(2));
        for idx in 0..MAX_THREADS {
            assert_eq!(thread_set.contains(idx), idx == 2);
        }
    }
}