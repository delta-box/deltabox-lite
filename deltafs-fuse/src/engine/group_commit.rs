use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use super::{EngineError, Result};

struct Request {
    path: PathBuf,
    response: Sender<std::result::Result<(), String>>,
}

#[derive(Debug, Clone)]
pub(crate) struct GroupCommitter {
    requests: Sender<Request>,
}

#[derive(Debug, Default)]
pub(crate) struct ReturnBarrier {
    tickets: Vec<Receiver<std::result::Result<(), String>>>,
}

impl ReturnBarrier {
    pub(crate) fn wait(self) -> Result<()> {
        for ticket in self.tickets {
            ticket
                .recv()
                .map_err(|_| EngineError::Invariant("group commit worker stopped".into()))?
                .map_err(|error| EngineError::Io(std::io::Error::other(error)))?;
        }
        Ok(())
    }
}

impl GroupCommitter {
    pub(crate) fn new(window: Duration) -> Self {
        let (requests, receiver) = mpsc::channel();
        thread::spawn(move || run(receiver, window));
        Self { requests }
    }

    fn submit(&self, path: PathBuf) -> Result<Receiver<std::result::Result<(), String>>> {
        let (response, ticket) = mpsc::channel();
        self.requests
            .send(Request { path, response })
            .map_err(|_| EngineError::Invariant("group commit worker stopped".into()))?;
        Ok(ticket)
    }
}

fn run(receiver: Receiver<Request>, window: Duration) {
    while let Ok(first) = receiver.recv() {
        thread::sleep(window);
        let mut batch = vec![first];
        batch.extend(receiver.try_iter());

        let mut outcomes = BTreeMap::new();
        for request in &batch {
            outcomes.entry(request.path.clone()).or_insert_with(|| {
                super::storage::sync_data(&request.path).map_err(|error| error.to_string())
            });
        }
        for request in batch {
            let result = outcomes
                .get(&request.path)
                .expect("batch path has a sync result")
                .clone();
            let _ = request.response.send(result);
        }
    }
}

impl ReturnBarrier {
    fn push(&mut self, ticket: Receiver<std::result::Result<(), String>>) {
        self.tickets.push(ticket);
    }
}

pub(crate) fn submit(
    committer: &GroupCommitter,
    barrier: &mut ReturnBarrier,
    path: PathBuf,
) -> Result<()> {
    barrier.push(committer.submit(path)?);
    Ok(())
}
