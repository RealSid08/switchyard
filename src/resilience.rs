//! Bounded cooldowns and durable response affinity. Neither map contains prompts.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct Resilience {
    cooling: HashMap<(String, String), Instant>,
    affinity: HashMap<String, (String, Instant)>,
    db: Option<rusqlite::Connection>,
}
impl Resilience {
    pub fn open(path: &std::path::Path) -> Result<Self, rusqlite::Error> {
        let db = rusqlite::Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS response_affinity(response TEXT PRIMARY KEY, connection TEXT NOT NULL, expires INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS affinity_expiry ON response_affinity(expires);")?;
        let timestamp = chrono::Utc::now().timestamp();
        db.execute(
            "DELETE FROM response_affinity WHERE expires<=?",
            [timestamp],
        )?;
        let mut affinity = HashMap::new();
        {
            let mut statement = db.prepare("SELECT response,connection,expires FROM response_affinity ORDER BY expires DESC LIMIT 4096")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (response, connection, expires) = row?;
                affinity.insert(
                    response,
                    (
                        connection,
                        Instant::now()
                            + Duration::from_secs((expires - timestamp).clamp(0, 3600) as u64),
                    ),
                );
            }
        }
        Ok(Self {
            cooling: HashMap::new(),
            affinity,
            db: Some(db),
        })
    }

    pub fn cool(&mut self, connection: &str, model: &str, seconds: u64) {
        self.prune();
        if self.cooling.len() >= 4096
            && let Some(old) = self
                .cooling
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone())
        {
            self.cooling.remove(&old);
        }
        self.cooling.insert(
            (connection.into(), model.into()),
            Instant::now() + Duration::from_secs(seconds.clamp(1, 3600)),
        );
    }
    pub fn cool_account(&mut self, connection: &str, seconds: u64) {
        self.cool(connection, "*", seconds);
    }
    pub fn reset(&mut self, connection: &str) {
        self.cooling.retain(|(c, _), _| c != connection);
    }
    pub fn remaining(&mut self, connection: &str, model: &str) -> u64 {
        self.prune();
        self.cooling
            .get(&(connection.into(), model.into()))
            .into_iter()
            .chain(self.cooling.get(&(connection.into(), "*".into())))
            .max()
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs() + 1)
            .unwrap_or(0)
    }
    pub fn cooldowns(&mut self, connection: &str) -> Vec<(String, u64)> {
        self.prune();
        let mut entries: Vec<_> = self
            .cooling
            .iter()
            .filter(|((account, _), _)| account == connection)
            .map(|((_, model), expiry)| {
                (
                    model.clone(),
                    expiry.saturating_duration_since(Instant::now()).as_secs() + 1,
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
    pub fn remember(&mut self, response: &str, connection: &str) {
        if response.is_empty() || response.len() > 256 || connection.len() > 256 {
            tracing::warn!("Response affinity identifier exceeds its size limit");
            return;
        }
        self.prune();
        if self.affinity.len() >= 4096
            && let Some(old) = self
                .affinity
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
        {
            self.affinity.remove(&old);
        }
        if let Some(db) = self.db.as_mut() {
            let persist = (|| -> Result<(), rusqlite::Error> {
                let tx = db.transaction()?;
                tx.execute("INSERT INTO response_affinity(response,connection,expires) VALUES(?,?,?) ON CONFLICT(response) DO UPDATE SET connection=excluded.connection,expires=excluded.expires", rusqlite::params![response,connection,chrono::Utc::now().timestamp()+3600])?;
                tx.execute("DELETE FROM response_affinity WHERE response NOT IN (SELECT response FROM response_affinity WHERE expires>? ORDER BY expires DESC LIMIT 4096)", [chrono::Utc::now().timestamp()])?;
                tx.commit()
            })();
            if persist.is_err() {
                tracing::error!(
                    "Could not persist response affinity; continuation routing may be lost after restart"
                );
            }
        }
        self.affinity.insert(
            response.into(),
            (
                connection.into(),
                Instant::now() + Duration::from_secs(3600),
            ),
        );
    }
    pub fn owner(&mut self, response: &str) -> Option<String> {
        self.prune();
        self.affinity.get(response).map(|(c, _)| c.clone())
    }
    fn prune(&mut self) {
        let now = Instant::now();
        self.cooling.retain(|_, t| *t > now);
        self.affinity.retain(|_, (_, t)| *t > now);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn affinity_survives_restart_without_response_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut first = Resilience::open(&path).unwrap();
        first.remember("resp_one", "account_a");
        drop(first);
        let mut restarted = Resilience::open(&path).unwrap();
        assert_eq!(restarted.owner("resp_one"), Some("account_a".into()));
    }
    #[test]
    fn affinity_is_account_specific() {
        let mut r = Resilience::default();
        r.remember("resp_one", "account_a");
        r.remember("resp_two", "account_b");
        assert_eq!(r.owner("resp_one"), Some("account_a".into()));
        assert_eq!(r.owner("missing"), None);
    }
    #[test]
    fn cooldown_is_model_specific_and_bounded() {
        let mut r = Resilience::default();
        r.cool("a", "m", u64::MAX);
        assert!((1..=3601).contains(&r.remaining("a", "m")));
        assert_eq!(r.remaining("a", "other"), 0);
        assert_eq!(r.remaining("b", "m"), 0);
    }
}
