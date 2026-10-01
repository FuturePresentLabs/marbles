//! One-shot, transactional migration of hosted company SQLite stores to PostgreSQL.
//!
//! This module deliberately does not make PostgreSQL another local-mode choice. SQLite remains
//! the boring workstation store; PostgreSQL is the hosted authority. The migration refuses to
//! merge into a populated tenant and verifies every table count before committing.

use std::path::{Path, PathBuf};

use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::X509;
use postgres::{Client, Transaction};
use postgres_openssl::MakeTlsConnector;
use rusqlite::Connection;
use serde::Serialize;

const SCHEMA: &str = include_str!("postgres_schema.sql");

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompanyReport {
    pub company_id: String,
    pub projects: i64,
    pub issues: i64,
    pub dependencies: i64,
    pub history_events: i64,
    pub outbound_events: i64,
}

pub fn connect(database_url: &str) -> Result<Client, String> {
    let connector =
        tls_connector().map_err(|e| format!("building PostgreSQL TLS connector: {e}"))?;
    Client::connect(database_url, connector)
        .map_err(|e| format!("connecting to PostgreSQL: {e}; cause: {e:?}"))
}

pub(crate) fn tls_connector() -> Result<MakeTlsConnector, openssl::error::ErrorStack> {
    let mut builder = SslConnector::builder(SslMethod::tls_client())?;
    builder.set_verify(SslVerifyMode::PEER);
    if let Ok(pem) = std::env::var("DATABASE_CA_PEM") {
        builder
            .cert_store_mut()
            .add_cert(X509::from_pem(pem.as_bytes())?)?;
    }
    Ok(MakeTlsConnector::new(builder.build()))
}

pub fn migrate_root(root: &Path, database_url: &str) -> Result<Vec<CompanyReport>, String> {
    let stores = discover_company_stores(root)?;
    if stores.is_empty() {
        return Err(format!("no company stores found under {}", root.display()));
    }
    let mut client = connect(database_url)?;
    client
        .batch_execute(SCHEMA)
        .map_err(|e| format!("installing PostgreSQL schema: {e}"))?;
    let mut reports = Vec::with_capacity(stores.len());
    for (company_id, path) in stores {
        reports.push(migrate_company(&mut client, &company_id, &path)?);
    }
    Ok(reports)
}

fn discover_company_stores(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|e| format!("reading company store root {}: {e}", root.display()))?;
    let mut stores = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading company store: {e}"))?;
        if !entry
            .file_type()
            .map_err(|e| format!("reading company store type: {e}"))?
            .is_dir()
        {
            continue;
        }
        let company_id = entry
            .file_name()
            .into_string()
            .map_err(|_| "company store name is not UTF-8".to_string())?;
        validate_company_id(&company_id)?;
        let database = entry.path().join("marbles.db");
        if database.is_file() {
            stores.push((company_id, database));
        }
    }
    stores.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(stores)
}

fn validate_company_id(company_id: &str) -> Result<(), String> {
    if company_id.is_empty()
        || company_id.len() > 128
        || !company_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(format!("unsafe company store name {company_id:?}"));
    }
    Ok(())
}

fn migrate_company(
    client: &mut Client,
    company_id: &str,
    sqlite_path: &Path,
) -> Result<CompanyReport, String> {
    let sqlite =
        Connection::open_with_flags(sqlite_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("opening {} read-only: {e}", sqlite_path.display()))?;
    let mut tx = client
        .transaction()
        .map_err(|e| format!("starting PostgreSQL transaction for {company_id}: {e}"))?;
    tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&company_id])
        .map_err(|e| format!("locking PostgreSQL tenant {company_id}: {e}"))?;
    let existing: i64 = tx
        .query_one(
            "SELECT COUNT(*) FROM marbles_project WHERE company_id=$1",
            &[&company_id],
        )
        .map_err(|e| format!("checking PostgreSQL tenant {company_id}: {e}"))?
        .get(0);
    if existing != 0 {
        return Err(format!(
            "refusing to merge {company_id}: PostgreSQL tenant already contains {existing} projects"
        ));
    }

    copy_projects(&sqlite, &mut tx, company_id)?;
    copy_issues(&sqlite, &mut tx, company_id)?;
    copy_dependencies(&sqlite, &mut tx, company_id)?;
    copy_history(&sqlite, &mut tx, company_id)?;
    copy_outbound_events(&sqlite, &mut tx, company_id)?;

    let report = CompanyReport {
        company_id: company_id.to_string(),
        projects: verified_count(&sqlite, &mut tx, company_id, "project", "marbles_project")?,
        issues: verified_count(&sqlite, &mut tx, company_id, "issue", "marbles_issue")?,
        dependencies: verified_count(&sqlite, &mut tx, company_id, "dep", "marbles_dep")?,
        history_events: verified_count(&sqlite, &mut tx, company_id, "history", "marbles_history")?,
        outbound_events: verified_count(
            &sqlite,
            &mut tx,
            company_id,
            "outbound_event",
            "marbles_outbound_event",
        )?,
    };
    tx.commit()
        .map_err(|e| format!("committing PostgreSQL tenant {company_id}: {e}"))?;
    Ok(report)
}

fn verified_count(
    sqlite: &Connection,
    tx: &mut Transaction<'_>,
    company_id: &str,
    sqlite_table: &str,
    postgres_table: &str,
) -> Result<i64, String> {
    let source: i64 = sqlite
        .query_row(&format!("SELECT COUNT(*) FROM {sqlite_table}"), [], |row| {
            row.get(0)
        })
        .map_err(|e| format!("counting SQLite {sqlite_table}: {e}"))?;
    let target: i64 = tx
        .query_one(
            &format!("SELECT COUNT(*) FROM {postgres_table} WHERE company_id=$1"),
            &[&company_id],
        )
        .map_err(|e| format!("counting PostgreSQL {postgres_table}: {e}"))?
        .get(0);
    if source != target {
        return Err(format!(
            "verification failed for {company_id}/{sqlite_table}: source={source}, target={target}"
        ));
    }
    Ok(source)
}

fn copy_projects(
    sqlite: &Connection,
    tx: &mut Transaction<'_>,
    company: &str,
) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare("SELECT slug, root, prefix, policy_json, created_at FROM project ORDER BY slug")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let values: (String, String, String, String, i64) = (
            row.get(0).map_err(|e| e.to_string())?,
            row.get(1).map_err(|e| e.to_string())?,
            row.get(2).map_err(|e| e.to_string())?,
            row.get(3).map_err(|e| e.to_string())?,
            row.get(4).map_err(|e| e.to_string())?,
        );
        tx.execute("INSERT INTO marbles_project(company_id,slug,root,prefix,policy_json,created_at) VALUES($1,$2,$3,$4,$5,$6)", &[&company, &values.0, &values.1, &values.2, &values.3, &values.4]).map_err(|e| format!("copying project {}: {e}", values.0))?;
    }
    Ok(())
}

fn copy_issues(sqlite: &Connection, tx: &mut Transaction<'_>, company: &str) -> Result<(), String> {
    let sql = "SELECT id,project,title,description,status,issue_type,priority,parent,labels_json,assignee,actor_kind,claimed_at,expires_at,available_at,created_by,created_at,updated_at,closed_at,close_reason,evidence_json,metadata_json,external_ref FROM issue ORDER BY id";
    let mut stmt = sqlite.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let s = |i| r.get::<_, String>(i).map_err(|e| e.to_string());
        let os = |i| r.get::<_, Option<String>>(i).map_err(|e| e.to_string());
        let oi = |i| r.get::<_, Option<i64>>(i).map_err(|e| e.to_string());
        let id = s(0)?;
        let project = s(1)?;
        let title = s(2)?;
        let description = s(3)?;
        let status = s(4)?;
        let issue_type = s(5)?;
        let priority: i64 = r.get(6).map_err(|e| e.to_string())?;
        let parent = os(7)?;
        let labels = s(8)?;
        let assignee = os(9)?;
        let actor_kind = os(10)?;
        let claimed_at = oi(11)?;
        let expires_at = oi(12)?;
        let available_at = oi(13)?;
        let created_by = s(14)?;
        let created_at: i64 = r.get(15).map_err(|e| e.to_string())?;
        let updated_at: i64 = r.get(16).map_err(|e| e.to_string())?;
        let closed_at = oi(17)?;
        let close_reason = os(18)?;
        let evidence = s(19)?;
        let metadata = s(20)?;
        let external_ref = os(21)?;
        tx.execute("INSERT INTO marbles_issue(company_id,id,project,title,description,status,issue_type,priority,parent,labels_json,assignee,actor_kind,claimed_at,expires_at,available_at,created_by,created_at,updated_at,closed_at,close_reason,evidence_json,metadata_json,external_ref) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23)", &[&company,&id,&project,&title,&description,&status,&issue_type,&priority,&parent,&labels,&assignee,&actor_kind,&claimed_at,&expires_at,&available_at,&created_by,&created_at,&updated_at,&closed_at,&close_reason,&evidence,&metadata,&external_ref]).map_err(|e| format!("copying issue {id}: {e}"))?;
    }
    Ok(())
}

fn copy_dependencies(
    sqlite: &Connection,
    tx: &mut Transaction<'_>,
    company: &str,
) -> Result<(), String> {
    copy_two_strings(
        sqlite,
        tx,
        company,
        "SELECT issue_id,depends_on FROM dep ORDER BY issue_id,depends_on",
        "INSERT INTO marbles_dep(company_id,issue_id,depends_on) VALUES($1,$2,$3)",
    )
}

fn copy_two_strings(
    sqlite: &Connection,
    tx: &mut Transaction<'_>,
    company: &str,
    select: &str,
    insert: &str,
) -> Result<(), String> {
    let mut stmt = sqlite.prepare(select).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let a: String = row.get(0).map_err(|e| e.to_string())?;
        let b: String = row.get(1).map_err(|e| e.to_string())?;
        tx.execute(insert, &[&company, &a, &b])
            .map_err(|e| format!("copying row {a}: {e}"))?;
    }
    Ok(())
}

fn copy_history(
    sqlite: &Connection,
    tx: &mut Transaction<'_>,
    company: &str,
) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare("SELECT seq,issue_id,ts,actor,event,detail FROM history ORDER BY seq")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let seq: i64 = r.get(0).map_err(|e| e.to_string())?;
        let issue: String = r.get(1).map_err(|e| e.to_string())?;
        let ts: i64 = r.get(2).map_err(|e| e.to_string())?;
        let actor: String = r.get(3).map_err(|e| e.to_string())?;
        let event: String = r.get(4).map_err(|e| e.to_string())?;
        let detail: String = r.get(5).map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO marbles_history(company_id,seq,issue_id,ts,actor,event,detail) VALUES($1,$2,$3,$4,$5,$6,$7)",&[&company,&seq,&issue,&ts,&actor,&event,&detail]).map_err(|e|format!("copying history {seq}: {e}"))?;
    }
    Ok(())
}

fn copy_outbound_events(
    sqlite: &Connection,
    tx: &mut Transaction<'_>,
    company: &str,
) -> Result<(), String> {
    let mut stmt=sqlite.prepare("SELECT seq,issue_id,project,occurred_at,event,delivered_at,attempts,next_attempt_at,last_error FROM outbound_event ORDER BY seq").map_err(|e|e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let seq: i64 = r.get(0).map_err(|e| e.to_string())?;
        let issue: String = r.get(1).map_err(|e| e.to_string())?;
        let project: String = r.get(2).map_err(|e| e.to_string())?;
        let occurred: i64 = r.get(3).map_err(|e| e.to_string())?;
        let event: String = r.get(4).map_err(|e| e.to_string())?;
        let delivered: Option<i64> = r.get(5).map_err(|e| e.to_string())?;
        let attempts: i64 = r.get(6).map_err(|e| e.to_string())?;
        let next: i64 = r.get(7).map_err(|e| e.to_string())?;
        let error: Option<String> = r.get(8).map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO marbles_outbound_event(company_id,seq,issue_id,project,occurred_at,event,delivered_at,attempts,next_attempt_at,last_error) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",&[&company,&seq,&issue,&project,&occurred,&event,&delivered,&attempts,&next,&error]).map_err(|e|format!("copying outbound event {seq}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_company_directory_names() {
        assert!(validate_company_id("fpl-cloud").is_ok());
        assert!(validate_company_id("../other").is_err());
        assert!(validate_company_id("").is_err());
    }
}
