use deadpool_sqlite::{Config, Pool, Runtime};

pub struct DB {
    pool: Pool,
}

impl DB {
    pub async fn get_user(&self, username: String) -> color_eyre::Result<Option<(String, String)>> {
        let conn = self.pool.get().await?;
        conn.interact(|conn| {
            let mut stmt =
                conn.prepare("SELECT token, device_id FROM users WHERE username = ?1")?;
            let mut rows = stmt.query((username,))?;
            if let Some(row) = rows.next()? {
                color_eyre::Result::<Option<(String, String)>>::Ok(Some((row.get(0)?, row.get(1)?)))
            } else {
                color_eyre::Result::<Option<(String, String)>>::Ok(None)
            }
        })
        .await
        .unwrap()
    }

    pub async fn set_user(
        &self,
        username: String,
        access_token: String,
        device_id: String,
    ) -> color_eyre::Result<()> {
        let conn = self.pool.get().await?;
        conn.interact(|conn| {
            let mut stmt =
                conn.prepare("INSERT INTO users (username, token, device_id) VALUES (?1, ?2, ?3)")?;
            stmt.execute((username, access_token, device_id))?;
            Ok(())
        })
        .await
        .unwrap()
    }
}

pub async fn setup_db() -> color_eyre::Result<DB> {
    let cfg = Config::new("./store/sip_bridge.sqlite3");
    let pool = cfg.create_pool(Runtime::Tokio1).unwrap();
    let conn = pool.get().await?;
    let _ = conn
        .interact(|conn| {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS users (
                username TEXT NOT NULL PRIMARY KEY,
                token TEXT NOT NULL,
                device_id TEXT NOT NULL
            )",
                (),
            )
        })
        .await
        .expect("Failed to create users table");
    Ok(DB { pool })
}
