use serde::{Deserialize, Serialize};

use super::{EngineError, Result};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FaultPoint {
    AfterPreimage,
    AfterReady,
    DuringMutation,
    AfterMutation,
    AfterApplied,
    DuringCheckpointSeal,
    AfterCheckpointSeal,
    DuringRollback,
    BeforeCommitMarker,
    AfterCommitMarker,
}

#[derive(Debug, Clone, Default)]
pub struct FaultInjector {
    armed: Option<(FaultPoint, usize)>,
}

impl FaultInjector {
    pub fn once(point: FaultPoint) -> Self {
        Self {
            armed: Some((point, 1)),
        }
    }

    pub fn on_nth(point: FaultPoint, occurrence: usize) -> Self {
        Self {
            armed: Some((point, occurrence.max(1))),
        }
    }

    pub(crate) fn check(&mut self, point: FaultPoint) -> Result<()> {
        if self.take(point) {
            return Err(EngineError::InjectedFault(point));
        }
        Ok(())
    }

    pub(crate) fn take(&mut self, point: FaultPoint) -> bool {
        let Some((armed, remaining)) = self.armed.as_mut() else {
            return false;
        };
        if *armed != point {
            return false;
        }
        if *remaining > 1 {
            *remaining -= 1;
            return false;
        }
        self.armed = None;
        true
    }
}
