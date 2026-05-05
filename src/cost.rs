//! Cost tracking and budget enforcement.
//!
//! Records API invocation costs per session/user/topic and enforces
//! per-group budget limits. Ported from jedarden's Go cost_events schema.

use crate::error::{AppError, Result};
use rusqlite::{params, Connection};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct CostEvent {
    pub id: String,
    pub chat_id: i64,
    pub thread_id: i64,
    pub session_id: String,
    pub cost_usd: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub model: String,
    pub from_user_id: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct CostSummary {
    pub total_usd: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub event_count: i64,
}

#[derive(Debug, Clone)]
pub struct DailyCost {
    pub date: String,
    pub total_usd: f64,
    pub event_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStatus {
    Ok,
    Warning(u8),
    Exceeded,
}

pub struct CostStore {
    conn: Connection,
}

impl CostStore {
    pub fn new(config_dir: &Path) -> Result<Self> {
        let db_path = config_dir.join("costs.db");
        let conn = Connection::open(&db_path).map_err(|e| AppError::Database(e.to_string()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if db_path.exists() {
                let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
            }
        }

        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS cost_events (
                    id                    TEXT PRIMARY KEY,
                    chat_id               INTEGER NOT NULL,
                    thread_id             INTEGER NOT NULL,
                    session_id            TEXT NOT NULL,
                    cost_usd              REAL NOT NULL DEFAULT 0.0,
                    input_tokens          INTEGER NOT NULL DEFAULT 0,
                    output_tokens         INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
                    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                    model                 TEXT NOT NULL DEFAULT '',
                    from_user_id          INTEGER NOT NULL DEFAULT 0,
                    created_at            TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_cost_chat ON cost_events(chat_id);
                CREATE INDEX IF NOT EXISTS idx_cost_thread ON cost_events(chat_id, thread_id);
                CREATE INDEX IF NOT EXISTS idx_cost_user ON cost_events(from_user_id);
                CREATE INDEX IF NOT EXISTS idx_cost_date ON cost_events(created_at);
                ",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn record(&self, event: &CostEvent) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO cost_events
                 (id, chat_id, thread_id, session_id, cost_usd, input_tokens,
                  output_tokens, cache_read_tokens, cache_creation_tokens,
                  model, from_user_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    event.id,
                    event.chat_id,
                    event.thread_id,
                    event.session_id,
                    event.cost_usd,
                    event.input_tokens,
                    event.output_tokens,
                    event.cache_read_tokens,
                    event.cache_creation_tokens,
                    event.model,
                    event.from_user_id,
                    event.created_at,
                ],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn get_group_total(&self, chat_id: i64) -> Result<CostSummary> {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0), COUNT(*)
                 FROM cost_events WHERE chat_id = ?1",
                params![chat_id],
                |r| {
                    Ok(CostSummary {
                        total_usd: r.get(0)?,
                        total_input_tokens: r.get(1)?,
                        total_output_tokens: r.get(2)?,
                        event_count: r.get(3)?,
                    })
                },
            )
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn get_topic_total(&self, chat_id: i64, thread_id: i64) -> Result<CostSummary> {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0), COUNT(*)
                 FROM cost_events WHERE chat_id = ?1 AND thread_id = ?2",
                params![chat_id, thread_id],
                |r| {
                    Ok(CostSummary {
                        total_usd: r.get(0)?,
                        total_input_tokens: r.get(1)?,
                        total_output_tokens: r.get(2)?,
                        event_count: r.get(3)?,
                    })
                },
            )
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn get_user_total(&self, from_user_id: i64) -> Result<CostSummary> {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0), COUNT(*)
                 FROM cost_events WHERE from_user_id = ?1",
                params![from_user_id],
                |r| {
                    Ok(CostSummary {
                        total_usd: r.get(0)?,
                        total_input_tokens: r.get(1)?,
                        total_output_tokens: r.get(2)?,
                        event_count: r.get(3)?,
                    })
                },
            )
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn get_daily_costs(&self, chat_id: i64, days: u32) -> Result<Vec<DailyCost>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DATE(created_at) as day, SUM(cost_usd), COUNT(*)
                 FROM cost_events
                 WHERE chat_id = ?1
                   AND created_at >= DATE('now', ?2)
                 GROUP BY day
                 ORDER BY day DESC",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let offset = format!("-{days} days");
        let rows = stmt
            .query_map(params![chat_id, offset], |r| {
                Ok(DailyCost {
                    date: r.get(0)?,
                    total_usd: r.get(1)?,
                    event_count: r.get(2)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| AppError::Database(e.to_string()))?);
        }
        Ok(out)
    }

    /// Check budget status. Returns Ok/Warning(pct)/Exceeded.
    pub fn check_budget(&self, chat_id: i64, max_budget_usd: f64) -> Result<BudgetStatus> {
        if max_budget_usd <= 0.0 {
            return Ok(BudgetStatus::Ok);
        }

        let summary = self.get_group_total(chat_id)?;
        let pct = ((summary.total_usd / max_budget_usd) * 100.0) as u8;

        if summary.total_usd >= max_budget_usd {
            Ok(BudgetStatus::Exceeded)
        } else if pct >= 80 {
            Ok(BudgetStatus::Warning(pct))
        } else {
            Ok(BudgetStatus::Ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_store() -> (CostStore, tempfile::TempDir) {
        let tmp = tempdir().expect("tempdir");
        let store = CostStore::new(tmp.path()).expect("CostStore::new");
        (store, tmp)
    }

    fn make_event(chat_id: i64, thread_id: i64, cost: f64, user_id: i64) -> CostEvent {
        CostEvent {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id,
            thread_id,
            session_id: "test-session".to_string(),
            cost_usd: cost,
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 10,
            cache_creation_tokens: 5,
            model: "sonnet".to_string(),
            from_user_id: user_id,
            created_at: chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }

    #[test]
    fn test_record_and_query() {
        let (store, _tmp) = make_store();
        store.record(&make_event(-100, 1, 0.05, 123)).unwrap();
        store.record(&make_event(-100, 1, 0.03, 123)).unwrap();
        store.record(&make_event(-100, 2, 0.10, 456)).unwrap();

        let group = store.get_group_total(-100).unwrap();
        assert!((group.total_usd - 0.18).abs() < 0.001);
        assert_eq!(group.event_count, 3);

        let topic = store.get_topic_total(-100, 1).unwrap();
        assert!((topic.total_usd - 0.08).abs() < 0.001);
        assert_eq!(topic.event_count, 2);

        let user = store.get_user_total(123).unwrap();
        assert!((user.total_usd - 0.08).abs() < 0.001);
    }

    #[test]
    fn test_budget_check() {
        let (store, _tmp) = make_store();
        store.record(&make_event(-100, 1, 5.0, 123)).unwrap();

        assert_eq!(store.check_budget(-100, 0.0).unwrap(), BudgetStatus::Ok);
        assert_eq!(store.check_budget(-100, 10.0).unwrap(), BudgetStatus::Ok);
        assert!(matches!(
            store.check_budget(-100, 6.0).unwrap(),
            BudgetStatus::Warning(_)
        ));
        assert_eq!(
            store.check_budget(-100, 5.0).unwrap(),
            BudgetStatus::Exceeded
        );
        assert_eq!(
            store.check_budget(-100, 3.0).unwrap(),
            BudgetStatus::Exceeded
        );
    }

    #[test]
    fn test_empty_group() {
        let (store, _tmp) = make_store();
        let summary = store.get_group_total(-999).unwrap();
        assert!((summary.total_usd).abs() < 0.001);
        assert_eq!(summary.event_count, 0);
    }
}
