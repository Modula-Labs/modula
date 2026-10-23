//! A scriptable [`Executor`] for tests: canned answers per operation, and a
//! record of every call (with the target it would have connected to).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::{DbError, Executor, Op, Target};

#[derive(Default)]
pub struct FakeDb {
    /// Operation name → its answer. Unset operations answer `{}`.
    pub answers: Mutex<HashMap<String, Result<Value, DbError>>>,
    pub calls: Mutex<Vec<(Target, Op)>>,
}

impl FakeDb {
    pub fn answer(&self, op: &str, answer: Result<Value, DbError>) {
        self.answers
            .lock()
            .expect("fake lock")
            .insert(op.to_owned(), answer);
    }

    pub fn calls(&self) -> Vec<(Target, Op)> {
        self.calls.lock().expect("fake lock").clone()
    }
}

#[async_trait]
impl Executor for FakeDb {
    async fn run(&self, target: &Target, op: Op, _timeout: Duration) -> Result<Value, DbError> {
        let answer = self
            .answers
            .lock()
            .expect("fake lock")
            .get(op.name())
            .cloned()
            .unwrap_or_else(|| Ok(serde_json::json!({})));
        self.calls
            .lock()
            .expect("fake lock")
            .push((target.clone(), op));
        answer
    }
}
