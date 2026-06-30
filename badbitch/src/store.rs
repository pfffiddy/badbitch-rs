//! Case storage — ports `_db` + save/load/list/export (badbitch2.py:255-323).
//! Same schema as the Python tool, but a separate `*_rs.sqlite` file (see config::rs_db_path).

use std::path::Path;

use rusqlite::Connection;

fn open(db_path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS cases(
            property_id TEXT PRIMARY KEY,
            address     TEXT,
            dossier_md  TEXT,
            updated     DATETIME DEFAULT CURRENT_TIMESTAMP)",
        [],
    )?;
    Ok(conn)
}

/// `save_dossier` (badbitch2.py:266): upsert the full Markdown dossier for a case.
pub fn save_dossier(db_path: &Path, property_id: &str, address: &str, dossier_md: &str) -> String {
    match (|| -> rusqlite::Result<()> {
        let conn = open(db_path)?;
        conn.execute(
            "INSERT INTO cases(property_id,address,dossier_md) VALUES(?1,?2,?3)
             ON CONFLICT(property_id) DO UPDATE SET
               address=excluded.address, dossier_md=excluded.dossier_md,
               updated=CURRENT_TIMESTAMP",
            rusqlite::params![property_id, address, dossier_md],
        )?;
        Ok(())
    })() {
        Ok(()) => format!("[saved] case '{property_id}' -> {}", db_path.display()),
        Err(e) => format!("[save error] {e}"),
    }
}

/// Load the raw case fields (address, dossier_md, updated) without Markdown re-formatting.
/// Used by exporters (e.g. Maltego/Graphviz) that need the body, not a display string.
pub fn load_raw(db_path: &Path, property_id: &str) -> rusqlite::Result<Option<(String, String, String)>> {
    let conn = open(db_path)?;
    Ok(conn
        .query_row(
            "SELECT address,dossier_md,updated FROM cases WHERE property_id=?1",
            rusqlite::params![property_id],
            |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .ok())
}

/// `load_dossier` (badbitch2.py:282).
pub fn load_dossier(db_path: &Path, property_id: &str) -> String {
    match (|| -> rusqlite::Result<Option<(String, String, String)>> {
        let conn = open(db_path)?;
        let row = conn
            .query_row(
                "SELECT address,dossier_md,updated FROM cases WHERE property_id=?1",
                rusqlite::params![property_id],
                |r| Ok((r.get::<_, String>(0).unwrap_or_default(), r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
            )
            .ok();
        Ok(row)
    })() {
        Ok(Some((addr, md, updated))) => format!("# {addr}  (updated {updated})\n\n{md}"),
        Ok(None) => format!("[not found] no saved case '{property_id}'"),
        Err(e) => format!("[load error] {e}"),
    }
}

/// `list_cases` (badbitch2.py:296).
pub fn list_cases(db_path: &Path) -> String {
    match (|| -> rusqlite::Result<Vec<(String, String, String)>> {
        let conn = open(db_path)?;
        let mut stmt =
            conn.prepare("SELECT property_id,address,updated FROM cases ORDER BY updated DESC")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1).unwrap_or_default(),
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })() {
        Ok(rows) if rows.is_empty() => "[no saved cases]".to_string(),
        Ok(rows) => rows
            .iter()
            .map(|(id, addr, upd)| format!("{id:<32} | {addr:<40} | {upd}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => format!("[list error] {e}"),
    }
}

/// `export_dossier` (badbitch2.py:309).
pub fn export_dossier(db_path: &Path, property_id: &str, out_path: &str, workdir: &Path) -> String {
    match (|| -> rusqlite::Result<Option<(String, String, String)>> {
        let conn = open(db_path)?;
        let row = conn
            .query_row(
                "SELECT address,dossier_md,updated FROM cases WHERE property_id=?1",
                rusqlite::params![property_id],
                |r| Ok((r.get::<_, String>(0).unwrap_or_default(), r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
            )
            .ok();
        Ok(row)
    })() {
        Ok(Some((addr, md, updated))) => {
            let path = if out_path.is_empty() {
                let safe = regex::Regex::new(r"[^\w.-]")
                    .unwrap()
                    .replace_all(property_id, "_")
                    .to_string();
                workdir.join(format!("{safe}.md"))
            } else {
                std::path::PathBuf::from(out_path)
            };
            match std::fs::write(&path, format!("# {addr}  (updated {updated})\n\n{md}\n")) {
                Ok(()) => format!("[exported] {}", path.display()),
                Err(e) => format!("[export error] {e}"),
            }
        }
        Ok(None) => format!("[not found] no saved case '{property_id}'"),
        Err(e) => format!("[export error] {e}"),
    }
}
