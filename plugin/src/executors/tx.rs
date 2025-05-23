use {
    std::{
        collections::{HashMap, HashSet},
        fmt::Debug,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    },
    bincode::serialize,
    antegen_network_program::state::{Pool, Registry, Worker},
    antegen_thread_program::state::VersionedThread,
    log::info,
    solana_client::{
        nonblocking::{rpc_client::RpcClient, tpu_client::TpuClient},
        rpc_config::RpcSimulateTransactionConfig,
        tpu_client::TpuClientConfig,
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPluginError, 
        Result as PluginResult,
    },
    solana_program::pubkey::Pubkey,
    solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool},
    solana_sdk::{
        commitment_config::CommitmentConfig,
        signature::{Keypair, Signature},
        transaction::{Transaction, VersionedTransaction},
    },
    tokio::{runtime::Runtime, sync::RwLock},
    crate::{config::PluginConfig, pool_position::PoolPosition, utils::read_or_new_keypair},
    super::AccountGet,
};

/// Number of slots to wait before checking for a confirmed transaction.
static TRANSACTION_CONFIRMATION_PERIOD: u64 = 24;

/// Number of slots to wait before trying to execute a thread while not in the pool.
static THREAD_TIMEOUT_WINDOW: u64 = 24;

/// Number of times to retry a thread simulation.
static MAX_THREAD_SIMULATION_FAILURES: u32 = 5;

/// The constant of the exponential backoff function.
static EXPONENTIAL_BACKOFF_CONSTANT: u32 = 2;

/// The number of slots to wait since the last rotation attempt.
static ROTATION_CONFIRMATION_PERIOD: u64 = 16;

/// TxExecutor
pub struct TxExecutor {
    pub config: PluginConfig,
    pub executable_threads: RwLock<HashMap<Pubkey, ExecutableThreadMetadata>>,
    pub transaction_history: RwLock<HashMap<Pubkey, TransactionMetadata>>,
    pub rotation_history: RwLock<Option<TransactionMetadata>>,
    pub dropped_threads: AtomicU64,
    pub keypair: Keypair,
}

#[derive(Debug)]
pub struct ExecutableThreadMetadata {
    pub due_slot: u64,
    pub simulation_failures: u32,
}

#[derive(Debug)]
pub struct TransactionMetadata {
    pub due_slot: u64,
    pub sent_slot: u64,
    pub signature: Signature,
}

impl TxExecutor {
    pub fn new(config: PluginConfig) -> Self {
        Self {
            config: config.clone(),
            executable_threads: RwLock::new(HashMap::new()),
            transaction_history: RwLock::new(HashMap::new()),
            rotation_history: RwLock::new(None),
            dropped_threads: AtomicU64::new(0),
            keypair: read_or_new_keypair(config.keypath),
        }
    }

    pub async fn execute_txs(
        self: Arc<Self>,
        client: Arc<RpcClient>,
        thread_pubkeys: HashSet<Pubkey>,
        slot: u64,
        runtime: Arc<Runtime>,
    ) -> PluginResult<()> {
        // Index the provided threads as executable.
        let mut w_executable_threads = self.executable_threads.write().await;
        thread_pubkeys.iter().for_each(|pubkey| {
            w_executable_threads.insert(
                *pubkey,
                ExecutableThreadMetadata {
                    due_slot: slot,
                    simulation_failures: 0,
                },
            );
        });

        // Drop threads that cross the simulation failure threshold.
        w_executable_threads.retain(|_thread_pubkey, metadata| {
            if metadata.simulation_failures > MAX_THREAD_SIMULATION_FAILURES {
                self.dropped_threads.fetch_add(1, Ordering::Relaxed);
                false
            } else {
                true
            }
        });
        info!(
            "dropped_threads: {:?} executable_threads: {:?}",
            self.dropped_threads.load(Ordering::Relaxed),
            *w_executable_threads
        );
        drop(w_executable_threads);

        // Process retries.
        self.clone()
            .process_retries(client.clone(), slot)
            .await
            .ok();

        // Get self worker's position in the delegate pool.
        let worker_pubkey = Worker::pubkey(self.config.worker_id);
        if let Ok(pool_position) = (client.get::<Pool>(&Pool::pubkey(0)).await).map(|pool| {
            let workers = &mut pool.workers.clone();
            PoolPosition {
                current_position: pool
                    .workers
                    .iter()
                    .position(|k| k.eq(&worker_pubkey))
                    .map(|i| i as u64),
                workers: workers.clone(),
            }
        }) {
            info!("pool_position: {:?}", pool_position);

            // Rotate into the worker pool.
            if pool_position.current_position.is_none() {
                self.clone()
                    .execute_pool_rotate_txs(client.clone(), slot, pool_position.clone())
                    .await
                    .ok();
            }

            // Execute thread transactions.
            self.clone()
                .execute_thread_exec_txs(client.clone(), slot, pool_position, runtime.clone())
                .await
                .ok();
        }

        Ok(())
    }

    async fn process_retries(
        self: Arc<Self>,
        client: Arc<RpcClient>,
        slot: u64,
    ) -> PluginResult<()> {
        // Get transaction signatures and corresponding threads to check.
        struct CheckableTransaction {
            due_slot: u64,
            thread_pubkey: Pubkey,
            signature: Signature,
        }
        let r_transaction_history = self.transaction_history.read().await;
        let checkable_transactions = r_transaction_history
            .iter()
            .filter(|(_, metadata)| slot > metadata.sent_slot + TRANSACTION_CONFIRMATION_PERIOD)
            .map(|(pubkey, metadata)| CheckableTransaction {
                due_slot: metadata.due_slot,
                thread_pubkey: *pubkey,
                signature: metadata.signature,
            })
            .collect::<Vec<CheckableTransaction>>();
        drop(r_transaction_history);

        // Lookup transaction statuses and track which threads are successful / retriable.
        let mut failed_threads: HashSet<Pubkey> = HashSet::new();
        let mut retriable_threads: HashSet<(Pubkey, u64)> = HashSet::new();
        let mut successful_threads: HashSet<Pubkey> = HashSet::new();
        for data in checkable_transactions {
            match client
                .get_signature_status_with_commitment(
                    &data.signature,
                    CommitmentConfig::processed(),
                )
                .await
            {
                Err(_err) => {}
                Ok(status) => match status {
                    None => {
                        info!(
                            "Retrying thread: {:?} missing_signature: {:?}",
                            data.thread_pubkey, data.signature
                        );
                        retriable_threads.insert((data.thread_pubkey, data.due_slot));
                    }
                    Some(status) => match status {
                        Err(err) => {
                            info!(
                                "Thread failed: {:?} failed_signature: {:?} err: {:?}",
                                data.thread_pubkey, data.signature, err
                            );
                            failed_threads.insert(data.thread_pubkey);
                        }
                        Ok(()) => {
                            successful_threads.insert(data.thread_pubkey);
                        }
                    },
                },
            }
        }

        // Requeue retriable threads and drop transactions from history.
        let mut w_transaction_history = self.transaction_history.write().await;
        let mut w_executable_threads = self.executable_threads.write().await;
        for pubkey in successful_threads {
            w_transaction_history.remove(&pubkey);
        }
        for pubkey in failed_threads {
            w_transaction_history.remove(&pubkey);
        }
        for (pubkey, due_slot) in retriable_threads {
            w_transaction_history.remove(&pubkey);
            w_executable_threads.insert(
                pubkey,
                ExecutableThreadMetadata {
                    due_slot,
                    simulation_failures: 0,
                },
            );
        }
        info!("transaction_history: {:?}", *w_transaction_history);
        drop(w_executable_threads);
        drop(w_transaction_history);
        Ok(())
    }

    async fn execute_pool_rotate_txs(
        self: Arc<Self>,
        client: Arc<RpcClient>,
        slot: u64,
        pool_position: PoolPosition,
    ) -> PluginResult<()> {
        let r_rotation_history = self.rotation_history.read().await;
        log::info!("Rotation history {:?}", r_rotation_history);
        let should_attempt = if r_rotation_history.is_some() {
            let rotation_history = r_rotation_history.as_ref().unwrap();
            if slot
                > rotation_history
                    .sent_slot
                    .checked_add(ROTATION_CONFIRMATION_PERIOD)
                    .unwrap()
            {
                if let Ok(sig_status) = client
                    .get_signature_status(&rotation_history.signature)
                    .await
                {
                    if let Some(status) = sig_status {
                        status.is_err()
                    } else {
                        true
                    }
                } else {
                    true
                }
            } else {
                false
            }
        } else {
            true
        };
        drop(r_rotation_history);
        if !should_attempt {
            return Ok(());
        }
        let registry = client.get::<Registry>(&Registry::pubkey()).await.unwrap();
        if let Some(tx) = crate::builders::build_pool_rotation_tx(
            client.clone(),
            &self.keypair,
            pool_position,
            registry,
            self.config.worker_id,
        )
        .await
        {
            self.clone().simulate_tx(&tx).await?;
            self.clone().submit_tx(&tx).await?;
            let mut w_rotation_history = self.rotation_history.write().await;
            *w_rotation_history = Some(TransactionMetadata {
                due_slot: slot,
                sent_slot: slot,
                signature: tx.signatures[0],
            });
            drop(w_rotation_history);
        }
        Ok(())
    }

    async fn get_executable_threads(
        self: Arc<Self>,
        pool_position: PoolPosition,
        slot: u64,
    ) -> PluginResult<Vec<(Pubkey, u64)>> {
        // Get the set of thread pubkeys that are executable.
        // Note we parallelize using rayon because this work is CPU heavy.
        let r_executable_threads = self.executable_threads.read().await;
        let thread_pubkeys =
            if pool_position.current_position.is_none() && !pool_position.workers.is_empty() {
                // This worker is not in the pool. Get pubkeys of threads that are beyond the timeout window.
                r_executable_threads
                    .iter()
                    .filter(|(_pubkey, metadata)| slot > metadata.due_slot + THREAD_TIMEOUT_WINDOW)
                    .filter(|(_pubkey, metadata)| slot >= exponential_backoff_threshold(*metadata))
                    .map(|(pubkey, metadata)| (*pubkey, metadata.due_slot))
                    .collect::<Vec<(Pubkey, u64)>>()
            } else {
                // This worker is in the pool. Get pubkeys executable threads.
                r_executable_threads
                    .iter()
                    .filter(|(_pubkey, metadata)| slot >= exponential_backoff_threshold(*metadata))
                    .map(|(pubkey, metadata)| (*pubkey, metadata.due_slot))
                    .collect::<Vec<(Pubkey, u64)>>()
            };
        drop(r_executable_threads);
        Ok(thread_pubkeys)
    }

    async fn execute_thread_exec_txs(
        self: Arc<Self>,
        client: Arc<RpcClient>,
        observed_slot: u64,
        pool_position: PoolPosition,
        runtime: Arc<Runtime>,
    ) -> PluginResult<()> {
        let executable_threads = self
            .clone()
            .get_executable_threads(pool_position, observed_slot)
            .await?;
        if executable_threads.is_empty() {
            return Ok(());
        }

        // Build transactions in parallel.
        // Note we parallelize using tokio because this work is IO heavy (RPC simulation calls).
        let tasks: Vec<_> = executable_threads
            .iter()
            .map(|(thread_pubkey, due_slot)| {
                runtime.spawn(self.clone().try_build_thread_exec_tx(
                    client.clone(),
                    observed_slot,
                    *due_slot,
                    *thread_pubkey,
                ))
            })
            .collect();
        let mut executed_threads: HashMap<Pubkey, (Signature, u64)> = HashMap::new();

        // Serialize to wire transactions.
        let wire_txs = futures::future::join_all(tasks)
            .await
            .iter()
            .filter_map(|res| match res {
                Err(_err) => None,
                Ok(res) => match res {
                    None => None,
                    Some((pubkey, tx, due_slot)) => {
                        executed_threads.insert(*pubkey, (tx.signatures[0], *due_slot));
                        Some(tx)
                    }
                },
            })
            .map(|tx| serialize(tx).unwrap())
            .collect::<Vec<Vec<u8>>>();

        // Batch submit transactions to the leader.
        match get_tpu_client()
            .await
            .try_send_wire_transaction_batch(wire_txs)
            .await
        {
            Err(err) => {
                info!("Failed to sent transaction batch: {:?}", err);
            }
            Ok(()) => {
                info!("Sent batch transactions!");
                let mut w_executable_threads = self.executable_threads.write().await;
                let mut w_transaction_history = self.transaction_history.write().await;
                for (pubkey, (signature, due_slot)) in executed_threads {
                    w_executable_threads.remove(&pubkey);
                    w_transaction_history.insert(
                        pubkey,
                        TransactionMetadata {
                            due_slot,
                            sent_slot: observed_slot,
                            signature,
                        },
                    );
                }
                drop(w_executable_threads);
                drop(w_transaction_history);
            }
        }

        Ok(())
    }

    pub async fn try_build_thread_exec_tx(
        self: Arc<Self>,
        client: Arc<RpcClient>,
        observed_slot: u64,
        due_slot: u64,
        thread_pubkey: Pubkey,
    ) -> Option<(Pubkey, VersionedTransaction, u64)> {
        let thread = match client.clone().get::<VersionedThread>(&thread_pubkey).await {
            Err(_err) => {
                self.increment_simulation_failure(thread_pubkey).await;
                return None;
            }
            Ok(thread) => thread,
        };

        // Exit early if the thread has been executed after it became due.
        if let Some(exec_context) = thread.exec_context() {
            if exec_context.last_exec_at.gt(&due_slot) {
                // Drop thread from the executable set to avoid bloat.
                let mut w_executable_threads = self.executable_threads.write().await;
                w_executable_threads.remove(&thread_pubkey);
                drop(w_executable_threads);
                return None;
            }
        }

        if let Ok(tx) = crate::builders::build_thread_exec_tx(
            client.clone(),
            &self.keypair,
            due_slot,
            thread,
            thread_pubkey,
            self.config.worker_id,
        )
        .await
        {
            if let Some(tx) = tx {
                if self
                    .clone()
                    .dedupe_tx(observed_slot, thread_pubkey, &tx)
                    .await
                    .is_ok()
                {
                    Some((thread_pubkey, tx, due_slot))
                } else {
                    None
                }
            } else {
                self.increment_simulation_failure(thread_pubkey).await;
                None
            }
        } else {
            None
        }
    }

    pub async fn increment_simulation_failure(self: Arc<Self>, thread_pubkey: Pubkey) {
        let mut w_executable_threads = self.executable_threads.write().await;
        w_executable_threads
            .entry(thread_pubkey)
            .and_modify(|metadata| metadata.simulation_failures += 1);
        drop(w_executable_threads);
    }

    pub async fn dedupe_tx(
        self: Arc<Self>,
        slot: u64,
        thread_pubkey: Pubkey,
        tx: &VersionedTransaction,
    ) -> PluginResult<()> {
        let r_transaction_history = self.transaction_history.read().await;
        if let Some(metadata) = r_transaction_history.get(&thread_pubkey) {
            if metadata.signature.eq(&tx.signatures[0]) && metadata.sent_slot.le(&slot) {
                return Err(GeyserPluginError::Custom(format!("Transaction signature is a duplicate of a previously submitted transaction").into()));
            }
        }
        drop(r_transaction_history);
        Ok(())
    }

    async fn simulate_tx(self: Arc<Self>, tx: &Transaction) -> PluginResult<Transaction> {
        RpcClient::new_with_commitment(LOCAL_RPC_URL.into(), CommitmentConfig::processed())
            .simulate_transaction_with_config(
                tx,
                RpcSimulateTransactionConfig {
                    replace_recent_blockhash: false,
                    commitment: Some(CommitmentConfig::processed()),
                    ..RpcSimulateTransactionConfig::default()
                },
            )
            .await
            .map_err(|err| {
                GeyserPluginError::Custom(format!("Tx failed simulation: {}", err).into())
            })
            .map(|response| match response.value.err {
                None => Ok(tx.clone()),
                Some(err) => Err(GeyserPluginError::Custom(
                    format!(
                        "Tx failed simulation: {} Logs: {:#?}",
                        err, response.value.logs
                    )
                    .into(),
                )),
            })?
    }

    async fn submit_tx(self: Arc<Self>, tx: &Transaction) -> PluginResult<Transaction> {
        if !get_tpu_client().await.send_transaction(tx).await {
            return Err(GeyserPluginError::Custom(
                "Failed to send transaction".into(),
            ));
        }
        Ok(tx.clone())
    }
}

impl Debug for TxExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tx-executor")
    }
}

fn exponential_backoff_threshold(metadata: &ExecutableThreadMetadata) -> u64 {
    metadata.due_slot + EXPONENTIAL_BACKOFF_CONSTANT.pow(metadata.simulation_failures) as u64 - 1
}

static LOCAL_RPC_URL: &str = "http://127.0.0.1:8899";
static LOCAL_WEBSOCKET_URL: &str = "ws://127.0.0.1:8900";

// Do not use a static ref here.
// -> The quic connections are dropped only when TpuClient is dropped
async fn get_tpu_client() -> TpuClient<QuicPool, QuicConnectionManager, QuicConfig> {
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        LOCAL_RPC_URL.into(),
        CommitmentConfig::processed(),
    ));
    let tpu_client = TpuClient::new(
        "tpu_client",
        rpc_client,
        LOCAL_WEBSOCKET_URL.into(),
        TpuClientConfig { fanout_slots: TRANSACTION_CONFIRMATION_PERIOD },
    )
    .await
    .unwrap();
    tpu_client
}
