use std::{collections::VecDeque, sync::Arc};

use serde_json::Value;
use tokio::sync::{RwLock, broadcast, watch};

use crate::model::{RuntimeState, SemanticEvent, unix_ms};

const EVENT_HISTORY_LIMIT: usize = 100;

#[derive(Clone)]
pub struct Runtime {
    state: Arc<RwLock<RuntimeState>>,
    state_tx: watch::Sender<RuntimeState>,
    events_tx: broadcast::Sender<SemanticEvent>,
    history: Arc<RwLock<VecDeque<SemanticEvent>>>,
    sequence: Arc<std::sync::atomic::AtomicU64>,
    telemetry: Arc<RwLock<crate::telemetry::Telemetry>>,
    worker_telemetry: Arc<RwLock<Option<crate::telemetry::WorkerTelemetry>>>,
}

impl Runtime {
    pub fn new() -> Self {
        let initial = RuntimeState::default();
        let (state_tx, _) = watch::channel(initial.clone());
        let (events_tx, _) = broadcast::channel(128);
        Self {
            state: Arc::new(RwLock::new(initial)),
            state_tx,
            events_tx,
            history: Arc::new(RwLock::new(VecDeque::with_capacity(EVENT_HISTORY_LIMIT))),
            sequence: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            telemetry: Arc::new(RwLock::new(Default::default())),
            worker_telemetry: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn state(&self) -> RuntimeState {
        self.state.read().await.clone()
    }

    pub async fn telemetry(&self) -> crate::telemetry::Telemetry {
        self.telemetry.read().await.clone()
    }

    pub async fn set_telemetry(&self, sample: crate::telemetry::Telemetry) {
        *self.telemetry.write().await = sample;
    }

    pub async fn worker_telemetry(&self) -> Option<crate::telemetry::WorkerTelemetry> {
        self.worker_telemetry.read().await.clone()
    }

    pub async fn set_worker_telemetry(&self, sample: crate::telemetry::WorkerTelemetry) {
        *self.worker_telemetry.write().await = Some(sample);
    }

    pub async fn update(&self, mutate: impl FnOnce(&mut RuntimeState)) {
        let mut state = self.state.write().await;
        mutate(&mut state);
        self.state_tx.send_replace(state.clone());
    }

    pub fn subscribe_state(&self) -> watch::Receiver<RuntimeState> {
        self.state_tx.subscribe()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<SemanticEvent> {
        self.events_tx.subscribe()
    }

    pub async fn recent_events(&self) -> Vec<SemanticEvent> {
        self.history.read().await.iter().cloned().collect()
    }

    pub async fn emit(
        &self,
        kind: impl Into<String>,
        source: impl Into<String>,
        confidence: Option<f32>,
        data: Value,
    ) -> SemanticEvent {
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let event = SemanticEvent {
            sequence,
            kind: kind.into(),
            source: source.into(),
            emitted_at_ms: unix_ms(),
            confidence,
            data,
        };
        {
            let mut history = self.history.write().await;
            if history.len() == EVENT_HISTORY_LIMIT {
                history.pop_front();
            }
            history.push_back(event.clone());
        }
        let _ = self.events_tx.send(event.clone());
        event
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
