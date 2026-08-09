//! Tracks subscribers to document read-sets and includes functionality to
//! notify subscribers on any changes to these documents.

use std::{
    collections::{
        hash_map::DefaultHasher,
        BTreeMap,
        HashMap,
    },
    future::Future,
    hash::{
        Hash,
        Hasher,
    },
    sync::{
        atomic::{
            AtomicBool,
            AtomicI64,
            AtomicUsize,
            Ordering,
        },
        Arc,
        OnceLock,
    },
    time::Duration,
};

use ::metrics::Timer;
use anyhow::Context;
use common::{
    bootstrap_model::index::database_index::IndexedFields,
    document_index_keys::{
        DatabaseIndexWrite,
        TextIndexWrite,
    },
    errors::report_error,
    knobs::{
        NUM_SUBSCRIPTION_MANAGERS,
        SUBSCRIPTIONS_WORKER_QUEUE_SIZE,
        SUBSCRIPTION_ADVANCE_LOG_TRACING_THRESHOLD,
        SUBSCRIPTION_INVALIDATION_DELAY_MULTIPLIER,
        SUBSCRIPTION_INVALIDATION_DELAY_THRESHOLD,
    },
    runtime::{
        block_in_place,
        Runtime,
        SpawnHandle,
    },
    types::{
        GenericIndexName,
        SubscriberId,
        TabletIndexName,
        Timestamp,
    },
};
use fastrace::future::FutureExt as _;
use futures::{
    future::BoxFuture,
    stream::FuturesUnordered,
    FutureExt as _,
    StreamExt as _,
};
use interval_map::IntervalMap;
use parking_lot::Mutex;
use prometheus::VMHistogram;
use search::query::{
    FilterConditionRead,
    TextQueryTermRead,
    TextSearchSubscription,
    TextSearchSubscriptions,
};
use slab::Slab;
use tokio::sync::{
    mpsc::{
        self,
        error::TrySendError,
    },
    oneshot,
    watch,
};
use value::TabletId;

use crate::{
    metrics::{
        self,
        log_subscriptions_invalidated,
    },
    reads::ReadSet,
    write_log::{
        LogOwner,
        LogReader,
        WriteInIndex,
        WriteSource,
    },
    Token,
};

pub struct InvalidationEvent {
    pub write_source: Option<WriteSource>,
    pub tablet_id: TabletId,
    /// Number of subscriptions invalidated.
    pub count: usize,
}

/// Holds a callback invoked after `advance_log` processes invalidations.
/// Set after construction since the callback target (`FunctionExecutionLog`)
/// is created after the database.
#[derive(Clone)]
pub struct InvalidationMetricCallback {
    inner: Arc<OnceLock<Arc<dyn Fn(Vec<InvalidationEvent>) + Send + Sync>>>,
}

impl InvalidationMetricCallback {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(OnceLock::new()),
        }
    }

    pub fn set(
        &self,
        callback: Arc<dyn Fn(Vec<InvalidationEvent>) + Send + Sync>,
    ) -> anyhow::Result<()> {
        self.inner
            .set(callback)
            .map_err(|_| anyhow::anyhow!("Invalidation callback already set"))
    }

    fn invoke(&self, events: Vec<InvalidationEvent>) {
        if let Some(callback) = self.inner.get() {
            callback(events);
        }
    }
}

type Sequence = usize;

/// Identifies a subscription entry within a `SubscriptionManager`. The `seq`
/// guards against stale releases after an entry was invalidated and
/// re-created: a release whose `seq` no longer matches the current entry is a
/// no-op.
#[derive(Clone, Copy, Debug)]
pub struct SubscriptionKey {
    id: SubscriberId,
    seq: Sequence,
}

#[derive(Clone)]
pub struct SubscriptionsClient {
    handles: Arc<Mutex<Vec<Box<dyn SpawnHandle>>>>,
    log: LogReader,
    senders: Vec<mpsc::Sender<SubscriptionRequest>>,
    next_manager: Arc<AtomicUsize>,
    // Deduplicates identical subscriptions across all clients: many clients
    // watching the same query share a single manager entry (one ReadSet, one
    // interval-map footprint) instead of each keeping their own copy.
    shared: Arc<Mutex<HashMap<DedupKey, Arc<SharedSubscriptionEntry>>>>,
}

impl SubscriptionsClient {
    pub fn subscribe(&self, token: Token, is_system: bool) -> anyhow::Result<Subscription> {
        let token = match self.log.refresh_reads_until_max_ts(token)? {
            Ok(t) => t,
            Err(invalid_ts) => return Ok(Subscription::invalid(invalid_ts)),
        };
        // Try to dedup with an existing identical subscription first.
        let key = (reads_digest(token.reads()), token.ts(), is_system);
        let entry = {
            let mut shared = self.shared.lock();
            match shared.get(&key).filter(|e| !e.is_invalid()) {
                Some(entry) => entry.clone(),
                None => {
                    let manager_idx =
                        self.next_manager.fetch_add(1, Ordering::Relaxed) % self.senders.len();
                    let (validity, valid_tx, valid_rx) = Subscription::new_parts(&token);
                    let entry = Arc::new(SharedSubscriptionEntry {
                        key,
                        validity,
                        valid_rx,
                        // Client handles increment this; the last drop (to 0)
                        // releases the manager entry.
                        users: Arc::new(AtomicUsize::new(0)),
                        subscription_key: Arc::new(Mutex::new(None)),
                        release_pending: Arc::new(AtomicBool::new(false)),
                        invalid: Arc::new(AtomicBool::new(false)),
                        release_tx: self.senders[manager_idx].clone(),
                        shared: self.shared.clone(),
                    });
                    // Watch for invalidation of the underlying subscription so
                    // the stale entry is evicted from the dedup map and future
                    // subscribes with the same key create a fresh one.
                    let watcher_entry = Arc::downgrade(&entry);
                    let mut watcher = entry.valid_rx.clone();
                    tokio::spawn(async move {
                        let _ = watcher
                            .wait_for(|state| matches!(state, SubscriptionState::Invalid))
                            .await;
                        if let Some(entry) = watcher_entry.upgrade() {
                            entry.mark_invalid();
                        }
                    });
                    let (_, sender) = Subscription::new_from_parts(
                        entry.validity.clone(),
                        valid_tx,
                        entry.valid_rx.clone(),
                    );
                    let (response_tx, response_rx) = oneshot::channel();
                    let request = SubscriptionRequest::Subscribe {
                        token,
                        sender,
                        is_system,
                        response: response_tx,
                    };
                    // Increment the counter first to avoid underflow
                    metrics::log_subscription_queue_length_delta(1);

                    // Round-robin selection of manager to handle this subscription
                    if let Err(e) = self.senders[manager_idx].try_send(request) {
                        metrics::log_subscription_queue_length_delta(-1);
                        shared.remove(&key);
                        return Err(match e {
                            TrySendError::Full(..) => {
                                metrics::subscriptions_worker_full_error().into()
                            },
                            TrySendError::Closed(..) => metrics::shutdown_error(),
                        });
                    }
                    // The manager acks with the SubscriptionKey assigned to the
                    // entry; a pending release (all clients dropped before the
                    // ack) is dispatched once we know it.
                    let ack_entry = entry.clone();
                    tokio::spawn(async move {
                        if let Ok(subscription_key) = response_rx.await {
                            ack_entry.ack(subscription_key);
                        }
                    });
                    shared.insert(key, entry.clone());
                    entry
                },
            }
        };
        Ok(entry.handle())
    }

    pub fn shutdown(&self) {
        for handle in self.handles.lock().iter_mut() {
            handle.shutdown();
        }
    }

    #[cfg(feature = "testing")]
    /// Number of dedup-map entries (test helper).
    pub fn test_shared_len(&self) -> usize {
        self.shared.lock().len()
    }
}

/// The other half of a `Subscription`, owned by the subscription worker.
/// On drop, this will invalidate the subscription.
pub struct SubscriptionSender {
    validity: Arc<Mutex<Validity>>,
    valid_tx: watch::Sender<SubscriptionState>,
}

impl Drop for SubscriptionSender {
    fn drop(&mut self) {
        // Make sure the subscription is marked invalid, but don't clobber any
        // existing `invalid_ts` if known
        if let ref mut validity @ Validity::Valid(_) = *self.validity.lock() {
            *validity = Validity::Invalid(None)
        }
        _ = self.valid_tx.send(SubscriptionState::Invalid);
    }
}

impl SubscriptionSender {
    fn drop_with_delay(self, delay: Option<Duration>, invalid_ts: Option<Timestamp>) {
        *self.validity.lock() = Validity::Invalid(invalid_ts);
        if let Some(delay) = delay {
            // Wait to invalidate the subscription by moving it into a new task
            tokio::spawn(async move {
                tokio::select! {
                    _ = self.valid_tx.closed() => (),
                    _ = tokio::time::sleep(delay) => (),
                }
                drop(self);
            });
        } else {
            drop(self);
        }
    }
}

enum SubscriptionRequest {
    Subscribe {
        token: Token,
        sender: SubscriptionSender,
        is_system: bool,
        /// Acks the manager-assigned `SubscriptionKey`, used to release the
        /// manager entry when the last shared handle is dropped.
        response: oneshot::Sender<SubscriptionKey>,
    },
    /// Release a shared subscription: removes the manager entry if it is
    /// still current (the seq guard makes this idempotent against entries
    /// that were already invalidated or re-created).
    Release { key: SubscriptionKey },
}

/// Deduplication key for identical subscriptions: a digest of the read set,
/// the (refreshed) timestamp, and whether the subscription is a system
/// subscription.
type DedupKey = (u64, Timestamp, bool);

/// A subscription shared by many clients (same query, same read set).
///
/// The manager holds exactly one entry (one ReadSet, one interval-map
/// footprint) for all of them; each client gets its own handle that shares
/// the validity and watch state. The entry is released when either the last
/// handle drops or the underlying subscription is invalidated by a write.
struct SharedSubscriptionEntry {
    key: DedupKey,
    validity: Arc<Mutex<Validity>>,
    // The original receiver, cloned for every handle. The entry itself does
    // not keep the channel alive after all handles are dropped, so the
    // manager's watch-close path still works as a backup to `Release`.
    valid_rx: watch::Receiver<SubscriptionState>,
    users: Arc<AtomicUsize>,
    subscription_key: Arc<Mutex<Option<SubscriptionKey>>>,
    release_pending: Arc<AtomicBool>,
    invalid: Arc<AtomicBool>,
    release_tx: mpsc::Sender<SubscriptionRequest>,
    shared: Arc<Mutex<HashMap<DedupKey, Arc<SharedSubscriptionEntry>>>>,
}

impl SharedSubscriptionEntry {
    fn is_invalid(&self) -> bool {
        self.invalid.load(Ordering::SeqCst)
    }

    /// The underlying subscription was invalidated by a write: evict the
    /// stale entry from the dedup map so future subscribes with the same key
    /// create a fresh one.
    fn mark_invalid(&self) {
        self.invalid.store(true, Ordering::SeqCst);
        self.evict();
    }

    /// Evict this entry from the dedup map. Safe to call more than once; if a
    /// newer entry for the same key was created in the meantime it is only
    /// removed from the map (cost: a missed dedup), never invalidated.
    fn evict(&self) {
        self.shared.lock().remove(&self.key);
    }

    fn ack(&self, subscription_key: SubscriptionKey) {
        *self.subscription_key.lock() = Some(subscription_key);
        if self.release_pending.load(Ordering::SeqCst) {
            // All clients dropped before the manager acked. The manager entry
            // is released below, so the dedup map must not keep a handle to a
            // dead subscription.
            self.evict();
            self.send_release(subscription_key);
        }
    }

    fn send_release(&self, subscription_key: SubscriptionKey) {
        let release_tx = self.release_tx.clone();
        tokio::spawn(async move {
            // Best effort: the manager's watch-close path cleans up if this
            // send is dropped (e.g. on shutdown).
            drop(release_tx.send(SubscriptionRequest::Release {
                key: subscription_key,
            }));
        });
    }

    /// A new client handle sharing this subscription.
    fn handle(&self) -> Subscription {
        self.users.fetch_add(1, Ordering::SeqCst);
        let users = self.users.clone();
        let key = self.key;
        let shared = self.shared.clone();
        let subscription_key = self.subscription_key.clone();
        let release_pending = self.release_pending.clone();
        let release_tx = self.release_tx.clone();
        Subscription {
            validity: self.validity.clone(),
            valid: self.valid_rx.clone(),
            _timer: metrics::subscription_timer(),
            on_drop: Some(Box::new(move || {
                // The last client drop releases the manager entry and evicts
                // this entry from the dedup map, so the next subscriber with
                // the same read set creates a fresh subscription. A handle
                // created from a stale map entry would otherwise watch a
                // channel whose manager-side subscription was already
                // released, and silently never receive invalidation events.
                if users.fetch_sub(1, Ordering::SeqCst) == 1 {
                    shared.lock().remove(&key);
                    match *subscription_key.lock() {
                        Some(key) => {
                            let release_tx = release_tx.clone();
                            tokio::spawn(async move {
                                drop(release_tx.send(SubscriptionRequest::Release { key }));
                            });
                        },
                        None => release_pending.store(true, Ordering::SeqCst),
                    }
                }
            })),
        }
    }
}

/// A digest of a read set for deduplication. Not cryptographic; equal read
/// sets hash equally because iteration order is deterministic.
fn reads_digest(reads: &ReadSet) -> u64 {
    let mut hasher = DefaultHasher::new();
    for (index, index_reads) in reads.iter_indexed() {
        index.hash(&mut hasher);
        index_reads.fields.hash(&mut hasher);
        for interval in index_reads.intervals.iter() {
            interval.hash(&mut hasher);
        }
    }
    for (index, search_reads) in reads.iter_search() {
        index.hash(&mut hasher);
        let text_queries: &Vec<TextQueryTermRead> = &search_reads.text_queries;
        text_queries.hash(&mut hasher);
        let filter_conditions: &Vec<FilterConditionRead> = &search_reads.filter_conditions;
        filter_conditions.hash(&mut hasher);
    }
    hasher.finish()
}

/// Tracks the minimum processed_ts across all SubscriptionManagers to
/// ensure the write log is only trimmed up to the point where all managers have
/// finished processing.
#[derive(Clone)]
struct RetentionCoordinator {
    /// Stores the processed_ts for each manager, indexed by manager id.
    processed_timestamps: Arc<Mutex<Vec<Timestamp>>>,
    log: Arc<Mutex<LogOwner>>,
}

impl RetentionCoordinator {
    fn new(num_managers: usize, initial_ts: Timestamp, log: LogOwner) -> Self {
        Self {
            processed_timestamps: Arc::new(Mutex::new(vec![initial_ts; num_managers])),
            log: Arc::new(Mutex::new(log)),
        }
    }

    fn update_and_enforce_retention(
        &self,
        manager_id: usize,
        processed_ts: Timestamp,
    ) -> anyhow::Result<()> {
        let min_ts = {
            let mut timestamps = self.processed_timestamps.lock();
            timestamps[manager_id] = processed_ts;
            *timestamps.iter().min().context("at least one manager")?
        };

        // We only need to enforce retention when the passed in processed_ts is the
        // minimum across all managers
        if min_ts == processed_ts {
            self.log.lock().enforce_retention_policy(min_ts);
        }
        Ok(())
    }
}

pub enum SubscriptionsWorker {}

impl SubscriptionsWorker {
    pub(crate) fn start<RT: Runtime>(
        log: LogOwner,
        runtime: RT,
        invalidation_callback: InvalidationMetricCallback,
    ) -> SubscriptionsClient {
        let num_managers = *NUM_SUBSCRIPTION_MANAGERS;
        let log_reader = log.reader();
        let initial_ts = log_reader.max_ts();

        let retention_coordinator = RetentionCoordinator::new(num_managers, initial_ts, log);

        let mut handles = Vec::with_capacity(num_managers);
        let mut senders = Vec::with_capacity(num_managers);

        for manager_id in 0..num_managers {
            let (tx, rx) = mpsc::channel(*SUBSCRIPTIONS_WORKER_QUEUE_SIZE);
            let rx = CountingReceiver(rx);

            let manager_log = log_reader.clone();
            let coordinator = retention_coordinator.clone();
            let mut manager = SubscriptionManager::new(
                manager_id,
                manager_log,
                coordinator,
                initial_ts,
                invalidation_callback.clone(),
            );
            let handle = runtime.spawn("subscription_worker", async move {
                manager.run_worker(rx).await
            });
            handles.push(handle);
            senders.push(tx);
        }

        SubscriptionsClient {
            handles: Arc::new(Mutex::new(handles)),
            log: log_reader,
            senders,
            next_manager: Arc::new(AtomicUsize::new(0)),
            shared: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

struct CountingReceiver(mpsc::Receiver<SubscriptionRequest>);
impl Drop for CountingReceiver {
    fn drop(&mut self) {
        self.0.close();
        metrics::log_subscription_queue_length_delta(-(self.0.len() as i64));
    }
}
impl CountingReceiver {
    async fn recv(&mut self) -> Option<SubscriptionRequest> {
        let r = self.0.recv().await;
        if r.is_some() {
            metrics::log_subscription_queue_length_delta(-1);
        }
        r
    }
}

impl SubscriptionManager {
    async fn run_worker(&mut self, mut rx: CountingReceiver) {
        tracing::info!("Starting subscriptions worker");
        loop {
            let processed_ts = self.processed_ts();
            futures::select_biased! {
                // N.B.: `futures` select macro (not `tokio`) needed for `select_next_some`
                key = self.closed_subscriptions.select_next_some() => {
                    self.remove(key);
                },
                request = rx.recv().fuse() => {
                    match request {
                        Some(SubscriptionRequest::Subscribe {
                            token,
                            sender,
                            is_system,
                            response,
                        }) => {
                            match self.subscribe(token, sender, is_system) {
                                Ok(key) => {
                                    let _ = response.send(key);
                                },
                                Err(mut e) => {
                                    report_error(&mut e).await;
                                },
                            }
                        },
                        Some(SubscriptionRequest::Release { key }) => {
                            // Idempotent: no-ops if the entry was already
                            // invalidated or re-created (seq mismatch).
                            self.remove(key);
                        },
                        None => {
                            tracing::info!("All clients have gone away, shutting down subscriptions worker...");
                            break;
                        },
                    }
                },
                next_ts = self.log.wait_for_higher_ts(processed_ts).fuse() => {
                    if let Err(mut e) = self.advance_log(next_ts) {
                        report_error(&mut e).await;
                    }
                },
            }
        }
    }
}

/// Tracks all subscribers to queries and the read-set they're watching for
/// updates on.
pub struct SubscriptionManager {
    /// Unique identifier for this manager (used for retention coordination)
    manager_id: usize,

    subscribers: Slab<Subscriber>,
    subscriptions: SubscriptionMap,
    next_seq: Sequence,

    closed_subscriptions: FuturesUnordered<BoxFuture<'static, SubscriptionKey>>,

    log: LogReader,

    retention_coordinator: RetentionCoordinator,

    // The timestamp until which the worker has processed the log, which may be lagging behind
    // `conflict_checker.max_ts()`.
    //
    // Invariant: All `ReadSet` in `subscribers` have a timestamp greater than or equal to
    // `processed_ts`.
    processed_ts: Arc<AtomicI64>,

    invalidation_callback: InvalidationMetricCallback,
}

struct Subscriber {
    reads: Arc<ReadSet>,
    sender: SubscriptionSender,
    seq: Sequence,
    is_system: bool,
}

impl SubscriptionManager {
    #[allow(unused)]
    fn new(
        manager_id: usize,
        log: LogReader,
        retention_coordinator: RetentionCoordinator,
        initial_ts: Timestamp,
        invalidation_callback: InvalidationMetricCallback,
    ) -> Self {
        Self {
            manager_id,
            subscribers: Slab::new(),
            subscriptions: SubscriptionMap::new(),
            next_seq: 0,
            closed_subscriptions: FuturesUnordered::new(),
            log,
            retention_coordinator,
            processed_ts: Arc::new(AtomicI64::new(initial_ts.into())),
            invalidation_callback,
        }
    }

    fn processed_ts(&self) -> Timestamp {
        Timestamp::try_from(self.processed_ts.load(Ordering::Relaxed))
            .expect("only valid Timestamp values are written to processed_ts")
    }

    pub fn subscribe(
        &mut self,
        mut token: Token,
        sender: SubscriptionSender,
        is_system: bool,
    ) -> anyhow::Result<SubscriptionKey> {
        metrics::log_subscription_queue_lag(self.log.max_ts().secs_since_f64(token.ts()));
        // The client may not have fully refreshed their token past our
        // processed timestamp, so finish the job for them if needed.
        //
        // Note that we allow tokens to go past the end of `self.processed_ts` if the
        // subscription worker is lagging far behind the client's
        // `refresh_reads` call. This is okay since we'll only duplicate
        // processing some log entries from `(self.processed_ts, token.ts()]`.
        let processed_ts = self.processed_ts();
        if token.ts() < processed_ts {
            token = match self.log.refresh_token(token, processed_ts)? {
                Ok(t) => t,
                Err(invalid_ts) => {
                    *sender.validity.lock() = Validity::Invalid(invalid_ts);
                    // N.B.: we only use the returned value for tests which
                    // don't encounter this case. The `id` is invalid so any
                    // later release is a no-op.
                    return Ok(SubscriptionKey {
                        id: usize::MAX,
                        seq: usize::MAX,
                    });
                },
            };
        }
        assert!(token.ts() >= processed_ts);

        let entry = self.subscribers.vacant_entry();
        let subscriber_id = entry.key();

        self.subscriptions.insert(subscriber_id, token.reads());

        let seq: usize = self.next_seq;
        let key = SubscriptionKey {
            id: subscriber_id,
            seq,
        };
        self.next_seq += 1;
        // Connect the subscription to this manager's `processed_ts`, so that
        // `subscription.current_ts()` automatically returns the latest
        // timestamp unless the subscription is explicitly invalidated.
        // Note that this can move the subscription's validity backward until
        // the next `advance_log`.
        sender.validity.lock().adopt(self.processed_ts.clone());
        let valid_tx = sender.valid_tx.clone();
        entry.insert(Subscriber {
            reads: token.reads_owned(),
            sender,
            seq,
            is_system,
        });
        self.closed_subscriptions.push(
            async move {
                valid_tx.closed().await;
                key
            }
            .boxed(),
        );
        Ok(key)
    }

    pub fn interval_map(&self, index_name: &TabletIndexName) -> Option<&IntervalMap> {
        self.subscriptions
            .indexed
            .get(index_name)
            .map(|(_, interval_map)| interval_map)
    }

    pub fn text_subscription_for_index(
        &self,
        index_name: &TabletIndexName,
    ) -> Option<&TextSearchSubscription> {
        self.subscriptions.search.get(index_name)
    }

    pub fn advance_log(&mut self, next_ts: Timestamp) -> anyhow::Result<()> {
        let _timer = metrics::subscriptions_update_timer();
        block_in_place(|| {
            let processed_ts = self.processed_ts();
            let from_ts = processed_ts.succ()?;

            // Maps subscriber_id -> (earliest invalidating write_ts, write_source,
            // tablet_id)
            let mut to_notify: BTreeMap<SubscriberId, (Timestamp, Option<WriteSource>, TabletId)> =
                BTreeMap::new();
            {
                let _timer = metrics::subscriptions_log_iterate_timer();
                let mut num_index_updates = 0;
                self.log.for_each_index(
                    from_ts,
                    next_ts,
                    &mut to_notify,
                    &mut num_index_updates,
                    |index_name, updates, to_notify, num_index_updates| {
                        if let Some(interval_map) = self.interval_map(index_name) {
                            Self::process_log_entry(
                                to_notify,
                                num_index_updates,
                                index_name,
                                |notify, num_index_updates| {
                                    Self::overlapping_database(
                                        interval_map,
                                        updates,
                                        notify,
                                        num_index_updates,
                                    )
                                },
                            );
                        }
                    },
                    |index_name, updates, to_notify, num_index_updates| {
                        if let Some(text_subscription) =
                            self.text_subscription_for_index(index_name)
                        {
                            Self::process_log_entry(
                                to_notify,
                                num_index_updates,
                                index_name,
                                |notify, num_index_updates| {
                                    self.overlapping_text(
                                        text_subscription,
                                        updates,
                                        notify,
                                        num_index_updates,
                                    )
                                },
                            );
                        }
                    },
                )?;
                metrics::log_subscriptions_processed_index_updates(num_index_updates);
                if _timer.elapsed()
                    > Duration::from_secs(*SUBSCRIPTION_ADVANCE_LOG_TRACING_THRESHOLD)
                {
                    let subscribers_by_index: BTreeMap<&GenericIndexName<_>, usize> = self
                        .subscriptions
                        .indexed
                        .iter()
                        .map(|(key, (_fields, range_map))| (key, range_map.subscriber_len()))
                        .collect();
                    let total_subscribers: usize = subscribers_by_index.values().sum();
                    let search_len = self.subscriptions.search.filter_len();
                    tracing::info!(
                        "[{next_ts} advance_log] Duration {}ms, indexes: {}, search filters: {}",
                        _timer.elapsed().as_millis(),
                        self.subscriptions.indexed.len(),
                        search_len,
                    );
                    tracing::info!(
                        "`[{next_ts} advance_log] Subscription map size: {total_subscribers}"
                    );
                    tracing::info!(
                        "[{next_ts} advance_log] Subscribers by index {subscribers_by_index:?}"
                    );
                }
            }

            {
                let _timer = metrics::subscriptions_invalidate_timer();
                // Notify invalidated subscriptions.
                let num_subscriptions_invalidated = to_notify.len();
                let should_splay_invalidations =
                    num_subscriptions_invalidated > *SUBSCRIPTION_INVALIDATION_DELAY_THRESHOLD;
                // N.B.: additionally multiply the delay by the number of
                // subscription workers, because the same widely-invalidating
                // commit most likely affects all of the workers equally.
                let splay_amt_millis = num_subscriptions_invalidated as u64
                    * *SUBSCRIPTION_INVALIDATION_DELAY_MULTIPLIER
                    * *NUM_SUBSCRIPTION_MANAGERS as u64;
                if should_splay_invalidations {
                    tracing::info!(
                        "Splaying subscription invalidations since there are {} subscriptions to \
                         invalidate. The threshold is {}. Splaying up to {} ms",
                        num_subscriptions_invalidated,
                        *SUBSCRIPTION_INVALIDATION_DELAY_THRESHOLD,
                        splay_amt_millis,
                    );
                }
                // Aggregate invalidation events by (write_source, tablet_id).
                // We use a Vec and aggregate manually since WriteSource doesn't
                // implement Ord.
                // Use display_name as the grouping key since WriteSource
                // doesn't implement Ord/Hash.
                let mut invalidation_counts: HashMap<
                    (Option<String>, TabletId),
                    (Option<WriteSource>, usize),
                > = HashMap::new();

                for (subscriber_id, (invalid_ts, write_source, tablet_id)) in to_notify {
                    let display_key = write_source.as_ref().and_then(|ws| ws.display_name());
                    let entry = invalidation_counts
                        .entry((display_key, tablet_id))
                        .or_insert_with(|| (write_source.clone(), 0));
                    entry.1 += 1;

                    let delay = if should_splay_invalidations {
                        let is_system_subscription = self
                            .subscribers
                            .get(subscriber_id)
                            .context("Missing subscriber")?
                            .is_system;
                        (!is_system_subscription).then(|| {
                            Duration::from_millis(rand::random_range(0..=splay_amt_millis))
                        })
                    } else {
                        None
                    };
                    self._remove(subscriber_id, delay, Some(invalid_ts));
                }
                log_subscriptions_invalidated(num_subscriptions_invalidated);

                // Invoke the invalidation callback with aggregated events.
                if !invalidation_counts.is_empty() {
                    let events: Vec<InvalidationEvent> = invalidation_counts
                        .into_iter()
                        .map(|((_display_key, tablet_id), (write_source, count))| {
                            InvalidationEvent {
                                write_source,
                                tablet_id,
                                count,
                            }
                        })
                        .collect();
                    self.invalidation_callback.invoke(events);
                }

                assert!(processed_ts <= next_ts);
                // Finally bump `processed_ts`. This automatically bumps the current_ts of all
                // adopted subscriptions.
                self.processed_ts.store(next_ts.into(), Ordering::Relaxed);
            }

            // Enforce retention after we have processed the subscriptions.
            // Use the coordinator to ensure we only trim up to the minimum
            // processed_ts across all managers.
            {
                let _timer = metrics::subscriptions_log_enforce_retention_timer();
                self.retention_coordinator
                    .update_and_enforce_retention(self.manager_id, next_ts)?;
            }

            Ok(())
        })
    }

    pub fn overlapping_database<'a, I>(
        interval_map: &IntervalMap,
        ordered_index_updates: I,
        notify: &mut (impl FnMut(SubscriberId, Timestamp, Option<WriteSource>) + ?Sized),
        num_index_updates: &mut usize,
    ) where
        I: Iterator<Item = &'a WriteInIndex<DatabaseIndexWrite>>,
    {
        for update in ordered_index_updates {
            let write_ts = update.ts;
            let write_source = &update.write_source;
            let mut notify_with_ts_and_write_source = |subscriber_id| {
                let write_source_clone = write_source.is_udf().then(|| write_source.clone());
                notify(subscriber_id, write_ts, write_source_clone)
            };
            for index_update in &update.index_updates {
                *num_index_updates += 1;
                for index_key in index_update.update.iter() {
                    interval_map.query(&index_key.0, &mut notify_with_ts_and_write_source);
                }
            }
        }
    }

    pub fn overlapping_text<'a, I>(
        &self,
        subscription: &TextSearchSubscription,
        ordered_index_updates: I,
        notify: &mut (impl FnMut(SubscriberId, Timestamp, Option<WriteSource>) + ?Sized),
        num_index_updates: &mut usize,
    ) where
        I: Iterator<Item = &'a WriteInIndex<TextIndexWrite>>,
    {
        for update in ordered_index_updates {
            let write_ts = update.ts;
            let write_source = &update.write_source;
            let mut notify_with_ts_and_write_source = |subscriber_id| {
                let write_source_clone = write_source.is_udf().then(|| write_source.clone());
                notify(subscriber_id, write_ts, write_source_clone)
            };
            for index_update in &update.index_updates {
                *num_index_updates += 1;
                for index_key in index_update.update.iter() {
                    self.subscriptions.search.add_matches(
                        subscription,
                        index_key,
                        &mut notify_with_ts_and_write_source,
                    );
                }
            }
        }
    }

    /// Shared logic for processing a single write log entry during
    /// `advance_log`. Builds the notify closure, calls `overlap_fn` to find
    /// overlapping subscriptions, and emits tracing if the entry was slow.
    fn process_log_entry(
        to_notify: &mut BTreeMap<SubscriberId, (Timestamp, Option<WriteSource>, TabletId)>,
        num_index_updates: &mut usize,
        index_name: &TabletIndexName,
        overlap_fn: impl FnOnce(
            &mut dyn FnMut(SubscriberId, Timestamp, Option<WriteSource>),
            &mut usize,
        ),
    ) {
        let tablet_id = *index_name.table();
        let mut notify = |subscriber_id, write_ts, write_source: Option<WriteSource>| {
            // Always take the earliest matching write_ts.
            // Since for_each iterates per-index (not per-ts),
            // we cannot rely on insertion order.
            to_notify
                .entry(subscriber_id)
                .and_modify(|e| {
                    if write_ts < e.0 {
                        *e = (write_ts, write_source.clone(), tablet_id);
                    }
                })
                .or_insert_with(|| (write_ts, write_source.clone(), tablet_id));
        };
        overlap_fn(&mut notify, num_index_updates);
    }

    fn get_subscriber(&self, key: SubscriptionKey) -> Option<&Subscriber> {
        let entry = self.subscribers.get(key.id)?;
        if entry.seq > key.seq {
            return None;
        }
        assert_eq!(entry.seq, key.seq);
        Some(entry)
    }

    /// Remove the given subscription if it exists.
    fn remove(&mut self, key: SubscriptionKey) {
        // Don't remove anything if `key` is no longer valid.
        if self.get_subscriber(key).is_none() {
            return;
        }
        self._remove(key.id, None, None);
    }

    fn _remove(
        &mut self,
        id: SubscriberId,
        delay: Option<Duration>,
        invalid_ts: Option<Timestamp>,
    ) {
        let entry = self.subscribers.remove(id);
        self.subscriptions.remove(id, &entry.reads);
        // dropping `entry.sender` will invalidate the subscription
        entry.sender.drop_with_delay(delay, invalid_ts);
    }
}

#[derive(Copy, Clone)]
enum SubscriptionState {
    Valid,
    Invalid,
}

enum Validity {
    Valid(Arc<AtomicI64>),
    Invalid(Option<Timestamp>),
}

impl Validity {
    fn valid(ts: Timestamp) -> Self {
        Self::Valid(Arc::new(AtomicI64::new(ts.into())))
    }

    fn invalid(invalid_ts: Option<Timestamp>) -> Validity {
        Self::Invalid(invalid_ts)
    }

    fn adopt(&mut self, validity_ts: Arc<AtomicI64>) {
        match self {
            Self::Valid(self_ts) => *self_ts = validity_ts,
            Self::Invalid(_) => panic!("cannot adopt an invalid subscription!"),
        }
    }

    fn valid_ts(&self) -> Option<Timestamp> {
        match self {
            Self::Valid(valid_ts) => Some(
                valid_ts
                    .load(Ordering::Relaxed)
                    .try_into()
                    .expect("only legal timestamp values can be written to valid_ts"),
            ),
            Self::Invalid(_) => None,
        }
    }

    fn invalid_ts(&self) -> Option<Timestamp> {
        match self {
            Self::Valid(_) => None,
            Self::Invalid(invalid_ts) => *invalid_ts,
        }
    }
}

/// A subscription on a set of read keys from a prior read-only transaction.
#[must_use]
pub struct Subscription {
    validity: Arc<Mutex<Validity>>,
    // May lag behind `validity` in case of subscription splaying
    valid: watch::Receiver<SubscriptionState>,
    _timer: Timer<VMHistogram>,
    // Invoked when this handle is dropped. Used by deduplicated
    // subscriptions to release the shared manager entry when the last client
    // disconnects.
    on_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
    }
}

impl Subscription {
    /// Creates the shared validity + watch state.
    fn new_parts(
        token: &Token,
    ) -> (
        Arc<Mutex<Validity>>,
        watch::Sender<SubscriptionState>,
        watch::Receiver<SubscriptionState>,
    ) {
        let validity = Arc::new(Mutex::new(Validity::valid(token.ts())));
        let (valid_tx, valid_rx) = watch::channel(SubscriptionState::Valid);
        (validity, valid_tx, valid_rx)
    }

    /// Assembles a `Subscription` + `SubscriptionSender` from shared state.
    fn new_from_parts(
        validity: Arc<Mutex<Validity>>,
        valid_tx: watch::Sender<SubscriptionState>,
        valid_rx: watch::Receiver<SubscriptionState>,
    ) -> (Self, SubscriptionSender) {
        let subscription = Subscription {
            validity: validity.clone(),
            valid: valid_rx,
            _timer: metrics::subscription_timer(),
            on_drop: None,
        };
        (subscription, SubscriptionSender { validity, valid_tx })
    }

    fn invalid(invalid_ts: Option<Timestamp>) -> Self {
        let (_, receiver) = watch::channel(SubscriptionState::Invalid);
        Subscription {
            validity: Arc::new(Mutex::new(Validity::invalid(invalid_ts))),
            valid: receiver,
            _timer: metrics::subscription_timer(),
            on_drop: None,
        }
    }

    pub fn current_ts(&self) -> Option<Timestamp> {
        self.validity.lock().valid_ts()
    }

    pub fn invalid_ts(&self) -> Option<Timestamp> {
        self.validity.lock().invalid_ts()
    }

    /// Wait for subscription invalidation. In general, prefer
    /// `Database::subscribe_and_wait_for_subscription_invalidation` to include
    /// metrics.
    pub fn wait_for_invalidation(&self) -> impl Future<Output = Option<Timestamp>> + use<> {
        let mut valid = self.valid.clone();
        let validity = self.validity.clone();
        let span = fastrace::Span::enter_with_local_parent("wait_for_invalidation");
        async move {
            let _: Result<_, _> = valid
                .wait_for(|state| matches!(state, SubscriptionState::Invalid))
                .await;
            validity.lock().invalid_ts()
        }
        .in_span(span)
    }
}

/// Tracks every subscriber for a given read-set.
struct SubscriptionMap {
    // TODO: remove nesting, merge all IntervalMaps into one big data structure
    indexed: BTreeMap<TabletIndexName, (IndexedFields, IntervalMap)>,
    search: TextSearchSubscriptions,
}

impl SubscriptionMap {
    fn new() -> Self {
        Self {
            indexed: BTreeMap::new(),
            search: TextSearchSubscriptions::new(),
        }
    }

    fn insert(&mut self, id: SubscriberId, reads: &ReadSet) {
        for (index, index_reads) in reads.iter_indexed() {
            let (_, interval_map) = self
                .indexed
                .entry(index.clone())
                .or_insert_with(|| (index_reads.fields.clone(), IntervalMap::new()));
            interval_map
                .insert(id, index_reads.intervals.iter())
                .expect("stored more than u32::MAX intervals?");
        }
        for (index, reads) in reads.iter_search() {
            self.search.insert(id, index, reads);
        }
    }

    fn remove(&mut self, id: SubscriberId, reads: &ReadSet) {
        for (index, _) in reads.iter_indexed() {
            let (_, range_map) = self
                .indexed
                .get_mut(index)
                .unwrap_or_else(|| panic!("Missing index entry for {index}"));
            range_map.remove(id);
            if range_map.is_empty() {
                self.indexed.remove(index);
            }
        }
        for (index, reads) in reads.iter_search() {
            self.search.remove(id, index, reads);
        }
    }
}
