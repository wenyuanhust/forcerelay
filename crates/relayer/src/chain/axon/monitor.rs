use std::sync::{Arc, RwLock};
use std::time::Duration;

use super::{contract::*, IBCInfoCache};
use crate::chain::axon::cache_ics_tx_hash_with_event;
use crate::event::bus::EventBus;
use crate::event::IbcEventWithHeight;
use crossbeam_channel as channel;
use ethers::contract::LogMeta;
use ethers::prelude::*;
use ethers::providers::Middleware;
use ethers::types::Address;
use ibc_relayer_types::Height;
use OwnableIBCHandler as Contract;
use OwnableIBCHandlerEvents as ContractEvents;

use crate::chain::tracking::TrackingId;
use crate::event::monitor::{Error, EventBatch, MonitorCmd, Next, Result, TxMonitorCmd};
use ibc_relayer_types::core::ics24_host::identifier::ChainId;
use tendermint_rpc::WebSocketClientUrl;
use tokio::runtime::Runtime as TokioRuntime;
use tracing::{debug, error, info, instrument, warn};

type Client = Provider<Ws>;

// #[derive(Clone, Debug)]
pub struct AxonEventMonitor {
    client: Arc<Client>,
    rt: Arc<TokioRuntime>,
    chain_id: ChainId,
    contract_address: Address,
    start_block_number: u64,
    rx_cmd: channel::Receiver<MonitorCmd>,
    event_bus: EventBus<Arc<Result<EventBatch>>>,
    ibc_cache: Arc<RwLock<IBCInfoCache>>,
    reprocess_events: Vec<(OwnableIBCHandlerEvents, LogMeta)>,
}

impl AxonEventMonitor {
    /// Create an event monitor, and connect to a node
    #[instrument(
        name = "axon_event_monitor.create",
        level = "error",
        skip_all,
        fields(chain = %chain_id, addr = %websocket_addr)
    )]
    pub fn new(
        chain_id: ChainId,
        websocket_addr: WebSocketClientUrl,
        contract_address: Address,
        rt: Arc<TokioRuntime>,
        ibc_cache: Arc<RwLock<IBCInfoCache>>,
    ) -> Result<(Self, TxMonitorCmd)> {
        let (tx_cmd, rx_cmd) = channel::unbounded();

        let client = rt
            .block_on(Provider::<Ws>::connect(websocket_addr.to_string()))
            .map_err(|_| Error::client_creation_failed(chain_id.clone(), websocket_addr))?;

        let start_block_number = rt
            .block_on(client.get_block_number())
            .map_err(|e| Error::others(e.to_string()))?
            .as_u64();

        let event_bus = EventBus::new();
        let monitor = Self {
            client: Arc::new(client),
            rt,
            chain_id,
            contract_address,
            start_block_number,
            rx_cmd,
            event_bus,
            ibc_cache,
            reprocess_events: vec![],
        };
        Ok((monitor, TxMonitorCmd::new(tx_cmd)))
    }

    #[allow(clippy::while_let_loop)]
    #[instrument(
        name = "axon_event_monitor",
        level = "error",
        skip_all,
        fields(chain = %self.chain_id)
    )]
    pub fn run(mut self) {
        if let Next::Continue = self.update_subscribe(false) {
            info!(
                "start Axon event monitor for {}, reprocess {} events",
                self.chain_id,
                self.reprocess_events.len()
            );
            (0..self.reprocess_events.len()).for_each(|_| {
                let (event, meta) = self.reprocess_events.remove(0);
                self.process_event(event, meta);
            });
            let mut contract = Contract::new(self.contract_address, Arc::clone(&self.client));
            info!(
                "start to fetch IBC events from block {}",
                self.start_block_number
            );
            loop {
                std::thread::sleep(Duration::from_secs(1));
                match self.run_once(&contract) {
                    (Next::Abort, _) => break,
                    (Next::Continue, false) => {
                        // recreate contract when WS connection meets error
                        contract = Contract::new(self.contract_address, Arc::clone(&self.client));
                        info!(
                            "re-start to fetch IBC events from block {}",
                            self.start_block_number
                        );
                    }
                    (Next::Continue, true) => {}
                }
            }
            debug!("event monitor is shutting down");
        }
    }

    pub fn restore_event_tx_hashes(
        &mut self,
        latest_block_count: u64,
    ) -> Result<Vec<IbcEventWithHeight>> {
        let contract = Arc::new(Contract::new(
            self.contract_address,
            Arc::clone(&self.client),
        ));
        let restore_block_number = self
            .start_block_number
            .checked_sub(latest_block_count)
            .ok_or(Error::others(format!(
                "latest_block_count {latest_block_count} exceeds start_block_number {}",
                self.start_block_number
            )))?;
        let event_filter = |event: &OwnableIBCHandlerEvents| {
            matches!(
                event,
                OwnableIBCHandlerEvents::SendPacketFilter(_)
                    | OwnableIBCHandlerEvents::WriteAcknowledgementFilter(_)
            )
        };
        let events = self
            .rt
            .block_on(
                contract
                    .events()
                    .from_block(restore_block_number)
                    .to_block(self.start_block_number)
                    .query_with_meta(),
            )
            .map_err(|e| Error::others(e.to_string()))?
            .into_iter()
            .map(|(event, meta)| {
                if event_filter(&event) {
                    self.reprocess_events.push((event.clone(), meta.clone()));
                }
                IbcEventWithHeight::new_with_tx_hash(
                    event.into(),
                    Height::from_noncosmos_height(meta.block_number.as_u64()),
                    meta.transaction_hash.into(),
                )
            })
            .collect::<Vec<_>>();
        debug!(
            "restored {} events on contract {}",
            events.len(),
            self.contract_address
        );
        Ok(events)
    }

    fn update_subscribe(&mut self, use_try: bool) -> Next {
        let cmd = if use_try {
            match self.rx_cmd.try_recv() {
                Ok(cmd) => cmd,
                Err(e) if e.is_disconnected() => return Next::Abort,
                // No command yet.
                Err(_) => return Next::Continue,
            }
        } else {
            match self.rx_cmd.recv() {
                Ok(cmd) => cmd,
                // Disconnected.
                Err(_) => return Next::Abort,
            }
        };
        match cmd {
            MonitorCmd::Shutdown => return Next::Abort,
            MonitorCmd::Subscribe(tx) => {
                if let Err(e) = tx.send(self.event_bus.subscribe()) {
                    error!("failed to send back subscription: {e}");
                }
            }
        }
        Next::Continue
    }

    fn run_once(&mut self, contract: &OwnableIBCHandler<Client>) -> (Next, bool) {
        if let Next::Abort = self.update_subscribe(true) {
            return (Next::Abort, true);
        }

        let tip_block_number = match self.rt.block_on(contract.client().get_block_number()) {
            Ok(tip) => tip.as_u64(),
            Err(err) => {
                error!("failed to fetch Axon latest block number: {err}");
                return (Next::Continue, false);
            }
        };

        if self.start_block_number >= tip_block_number {
            return (Next::Continue, true);
        }

        let query = contract
            .events()
            .from_block(self.start_block_number)
            .to_block(tip_block_number);
        let events = match self.rt.block_on(query.query_with_meta()) {
            Ok(events) => events,
            Err(err) => {
                error!(
                    "failed to fetch events from block {} to block {tip_block_number}: {err}",
                    self.start_block_number
                );
                return (Next::Continue, false);
            }
        };

        events
            .into_iter()
            .for_each(|(event, meta)| self.process_event(event, meta));

        self.start_block_number = tip_block_number + 1;
        (Next::Continue, true)
    }

    fn process_event(&mut self, event: ContractEvents, meta: LogMeta) {
        println!("\n{}\n[event] = {:?}", self.chain_id, event);
        println!("[event_meta] = {:?}\n", meta);

        self.start_block_number = meta.block_number.as_u64();
        let event = IbcEventWithHeight::new_with_tx_hash(
            event.into(),
            Height::from_noncosmos_height(meta.block_number.as_u64()),
            meta.transaction_hash.into(),
        );
        cache_ics_tx_hash_with_event(
            &mut self.ibc_cache.write().unwrap(),
            event.event.clone(),
            event.tx_hash,
        );
        let batch = EventBatch {
            chain_id: self.chain_id.clone(),
            tracking_id: TrackingId::Static("Axon solidity event streaming"),
            height: Height::from_noncosmos_height(meta.block_number.as_u64()),
            events: vec![event],
        };
        self.process_batch(batch);
    }

    fn process_batch(&mut self, batch: EventBatch) {
        self.event_bus.broadcast(Arc::new(Ok(batch)));
    }
}
