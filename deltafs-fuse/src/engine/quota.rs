use serde::{Deserialize, Serialize};

use super::{EngineError, Result};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaState {
    pub limit: u64,
    pub used: u64,
}

impl QuotaState {
    pub fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    pub fn reserve(&self, bytes: u64) -> Result<QuotaReservation> {
        let available = self.limit.saturating_sub(self.used);
        if bytes > available {
            return Err(EngineError::QuotaExceeded {
                requested: bytes,
                available,
            });
        }
        Ok(QuotaReservation { reserved: bytes })
    }

    pub fn commit(&mut self, reservation: QuotaReservation, actual: u64) -> Result<()> {
        if actual > reservation.reserved {
            return Err(EngineError::Invariant(format!(
                "quota actual {actual} exceeds reservation {}",
                reservation.reserved
            )));
        }
        self.used = self
            .used
            .checked_add(actual)
            .ok_or_else(|| EngineError::Invariant("quota usage overflow".into()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QuotaReservation {
    reserved: u64,
}

impl QuotaReservation {
    pub fn covers(self, bytes: u64) -> bool {
        self.reserved >= bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_precedes_usage() {
        let mut quota = QuotaState::new(100);
        let reservation = quota.reserve(80).unwrap();
        assert_eq!(quota.used, 0);
        quota.commit(reservation, 50).unwrap();
        assert_eq!(quota.used, 50);
        assert!(matches!(
            quota.reserve(51),
            Err(EngineError::QuotaExceeded { .. })
        ));
    }
}
