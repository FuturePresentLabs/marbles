//! Hosted PostgreSQL storage. Every statement carries `company_id`; the tenant
//! boundary is data, not connection state or a caller-controlled search path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{TimeZone, Utc};
use postgres::{Client, GenericClient, Transaction};

use crate::db::{
    CompanyStoreRegistry, Error, OutboundEvent, Result, Store, SweepReport, now,
    validate_company_id,
};
use crate::time::WorkWeek;
use crate::types::*;

pub struct PostgresCompanyStores {
    client: Arc<Mutex<Client>>,
    open: Mutex<BTreeMap<String, Arc<dyn Store>>>,
}

impl PostgresCompanyStores {
    pub fn connect(database_url: &str) -> Result<Self> {
        let connector = crate::postgres_migration::tls_connector()
            .map_err(|e| Error::Bad(format!("building PostgreSQL TLS connector: {e}")))?;
        let mut client = Client::connect(database_url, connector)?;
        client.batch_execute(include_str!("postgres_schema.sql"))?;
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            open: Mutex::new(BTreeMap::new()),
        })
    }
}

impl CompanyStoreRegistry for PostgresCompanyStores {
    fn for_company(&self, company_id: &str) -> Result<Arc<dyn Store>> {
        validate_company_id(company_id)?;
        let mut open = self
            .open
            .lock()
            .map_err(|_| Error::Bad("PostgreSQL company store lock poisoned".into()))?;
        if let Some(store) = open.get(company_id) {
            return Ok(Arc::clone(store));
        }
        let store: Arc<dyn Store> = Arc::new(PostgresStore {
            client: Arc::clone(&self.client),
            company_id: company_id.to_owned(),
            policy: Policy::default(),
            week: WorkWeek::default(),
        });
        open.insert(company_id.to_owned(), Arc::clone(&store));
        Ok(store)
    }

    fn sweep_open(&self, at: i64) -> Result<Vec<(String, SweepReport)>> {
        self.open_stores()?
            .into_iter()
            .map(|(company, store)| Ok((company, store.sweep(at)?)))
            .collect()
    }

    fn open_stores(&self) -> Result<Vec<(String, Arc<dyn Store>)>> {
        let open = self
            .open
            .lock()
            .map_err(|_| Error::Bad("PostgreSQL company store lock poisoned".into()))?;
        Ok(open
            .iter()
            .map(|(company, store)| (company.clone(), Arc::clone(store)))
            .collect())
    }
}

struct PostgresStore {
    client: Arc<Mutex<Client>>,
    company_id: String,
    policy: Policy,
    week: WorkWeek,
}

impl PostgresStore {
    fn client(&self) -> Result<std::sync::MutexGuard<'_, Client>> {
        self.client
            .lock()
            .map_err(|_| Error::Bad("PostgreSQL client lock poisoned".into()))
    }

    fn transaction<T>(&self, f: impl FnOnce(&mut Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut client = self.client()?;
        let mut tx = client.transaction()?;
        // Serialize one tenant's state machine across blue/green server processes. This also
        // makes the migrated, per-company history sequence safe without introducing a second
        // sequence namespace that could collide with preserved SQLite ids.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            &[&self.company_id],
        )?;
        let value = f(&mut tx)?;
        tx.commit()?;
        Ok(value)
    }
}

impl Store for PostgresStore {
    fn ensure_project(&self, slug: &str, root: &str, prefix: &str) -> Result<()> {
        let at = now();
        self.transaction(|tx| {
            tx.execute(
                "INSERT INTO marbles_project(company_id,slug,root,prefix,created_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(company_id,slug) DO UPDATE SET root=excluded.root",
                &[&self.company_id, &slug, &root, &prefix, &at],
            )?;
            Ok(())
        })
    }

    fn projects(&self) -> Result<Vec<(String, String, String)>> {
        let mut client = self.client()?;
        Ok(client
            .query(
                "SELECT slug,root,prefix FROM marbles_project WHERE company_id=$1 ORDER BY slug",
                &[&self.company_id],
            )?
            .into_iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect())
    }

    fn pending_events(&self, at: i64, limit: usize) -> Result<Vec<OutboundEvent>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut client = self.client()?;
        Ok(client.query("SELECT seq,issue_id,project,occurred_at,event,attempts FROM marbles_outbound_event WHERE company_id=$1 AND delivered_at IS NULL AND next_attempt_at <= $2 ORDER BY seq LIMIT $3", &[&self.company_id,&at,&limit])?.into_iter().map(|r| OutboundEvent { seq:r.get(0), issue_id:r.get(1), project:r.get(2), occurred_at:r.get(3), event:r.get(4), attempts:r.get(5) }).collect())
    }

    fn mark_event_delivered(&self, seq: i64, at: i64) -> Result<()> {
        self.transaction(|tx| {
            tx.execute("UPDATE marbles_outbound_event SET delivered_at=$3,attempts=attempts+1,last_error=NULL WHERE company_id=$1 AND seq=$2 AND delivered_at IS NULL", &[&self.company_id,&seq,&at])?;
            Ok(())
        })
    }

    fn mark_event_failed(&self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()> {
        self.transaction(|tx| {
            tx.execute("UPDATE marbles_outbound_event SET attempts=attempts+1,next_attempt_at=$3,last_error=$4 WHERE company_id=$1 AND seq=$2 AND delivered_at IS NULL", &[&self.company_id,&seq,&next_attempt_at,&error])?;
            Ok(())
        })
    }

    fn create(&self, spec: &NewIssue, at: i64) -> Result<String> {
        let project = spec
            .project
            .clone()
            .ok_or_else(|| Error::Bad("create requires a project".into()))?;
        let available = spec
            .available_at
            .unwrap_or(at + self.policy.creation_grace_seconds);
        self.transaction(|tx| {
            let prefix: String = tx.query_opt("SELECT prefix FROM marbles_project WHERE company_id=$1 AND slug=$2", &[&self.company_id,&project])?.ok_or_else(|| Error::NoProject(project.clone()))?.get(0);
            let id = if let Some(id) = &spec.id {
                if tx.query_opt("SELECT 1 FROM marbles_issue WHERE company_id=$1 AND id=$2", &[&self.company_id,id])?.is_some() { return Err(Error::Bad(format!("issue id {id} already exists"))); }
                id.clone()
            } else { loop {
                let candidate = format!("{prefix}-{}", short_suffix());
                if tx.query_opt("SELECT 1 FROM marbles_issue WHERE company_id=$1 AND id=$2", &[&self.company_id,&candidate])?.is_none() { break candidate; }
            }};
            let labels = serde_json::to_string(&spec.labels)?;
            let mut metadata = match spec.metadata.clone().unwrap_or_else(|| serde_json::json!({})) { serde_json::Value::Object(map) => map, _ => return Err(Error::Bad("create metadata must be a JSON object".into())) };
            if let Some(reference)=&spec.external_ref { metadata.insert("external_ref".into(), serde_json::json!(reference)); }
            let metadata = serde_json::to_string(&metadata)?;
            let priority = if spec.priority == 0 { 2 } else { spec.priority };
            let created_by = spec.created_by.clone().unwrap_or_default();
            tx.execute("INSERT INTO marbles_issue(company_id,id,project,title,description,status,issue_type,priority,parent,labels_json,available_at,created_by,created_at,updated_at,metadata_json,external_ref) VALUES($1,$2,$3,$4,$5,'open',$6,$7,$8,$9,$10,$11,$12,$12,$13,$14)", &[&self.company_id,&id,&project,&spec.title,&spec.description,&spec.issue_type,&priority,&spec.parent,&labels,&available,&created_by,&at,&metadata,&spec.external_ref])?;
            log_event(tx,&self.company_id,&id,at,&created_by,"created",&spec.title)?;
            Ok(id)
        })
    }

    fn get(&self, id: &str) -> Result<Issue> {
        let mut client = self.client()?;
        row_to_issue(&mut *client, &self.company_id, id)?.ok_or_else(|| Error::NotFound(id.into()))
    }

    fn list(&self, project: Option<&str>, all: bool, only_id: Option<&str>) -> Result<Vec<Issue>> {
        let mut client = self.client()?;
        let rows = client.query("SELECT id FROM marbles_issue WHERE company_id=$1 AND ($2::boolean OR status != 'closed') AND ($3::text IS NULL OR project=$3) AND ($4::text IS NULL OR id=$4) ORDER BY priority,created_at,id", &[&self.company_id,&all,&project,&only_id])?;
        rows.into_iter()
            .map(|r| {
                let id: String = r.get(0);
                row_to_issue(&mut *client, &self.company_id, &id)?.ok_or(Error::NotFound(id))
            })
            .collect()
    }

    fn ready(&self, project: Option<&str>, at: i64) -> Result<Vec<Issue>> {
        let mut client = self.client()?;
        let rows=client.query("SELECT i.id FROM marbles_issue i WHERE i.company_id=$1 AND i.status='open' AND (i.available_at IS NULL OR i.available_at <= $2) AND ($3::text IS NULL OR i.project=$3) AND NOT EXISTS(SELECT 1 FROM marbles_dep d JOIN marbles_issue p ON p.company_id=d.company_id AND p.id=d.depends_on WHERE d.company_id=i.company_id AND d.issue_id=i.id AND p.status!='closed') AND (i.assignee IS NULL OR (i.expires_at IS NOT NULL AND i.expires_at <= $2)) ORDER BY i.priority,i.created_at,i.id", &[&self.company_id,&at,&project])?;
        rows.into_iter()
            .map(|r| {
                let id: String = r.get(0);
                row_to_issue(&mut *client, &self.company_id, &id)?.ok_or(Error::NotFound(id))
            })
            .collect()
    }

    fn update(&self, id: &str, patch: &IssuePatch, actor: &str, at: i64) -> Result<Issue> {
        if let Some(status) = &patch.status {
            if !STATUSES.contains(&status.as_str()) {
                return Err(Error::BadStatus(status.clone()));
            }
        }
        self.transaction(|tx| {
            let mut issue=row_to_issue(tx,&self.company_id,id)?.ok_or_else(|| Error::NotFound(id.into()))?;
            if let Some(v)=&patch.title { issue.title=v.clone(); } if let Some(v)=&patch.description { issue.description=v.clone(); }
            if let Some(v)=&patch.append_notes { if !issue.description.is_empty(){issue.description.push('\n');} issue.description.push_str(v); }
            if let Some(v)=patch.priority { issue.priority=v; } if let Some(v)=&patch.evidence { issue.evidence=v.clone(); }
            if let Some(v)=&patch.metadata { let serde_json::Value::Object(incoming)=v else{return Err(Error::Bad("metadata patch must be a JSON object".into()))}; let mut map=issue.metadata.as_object().cloned().unwrap_or_default(); for (k,v) in incoming { if v.is_null(){map.remove(k);}else{map.insert(k.clone(),v.clone());}} issue.metadata=serde_json::Value::Object(map); issue.external_ref=issue.metadata.get("external_ref").and_then(|v|v.as_str()).map(str::to_owned); }
            let status_changed=patch.status.as_deref().is_some_and(|s|s!=issue.status); if let Some(v)=&patch.status {issue.status=v.clone();issue.closed_at=(v=="closed").then_some(at);}
            let available=patch.available_at.unwrap_or_else(|| (at+self.policy.edit_deferral_seconds).max(issue.available_at.unwrap_or(0)));
            let labels=serde_json::to_string(&issue.labels)?; let evidence=serde_json::to_string(&issue.evidence)?; let metadata=serde_json::to_string(&issue.metadata)?;
            tx.execute("UPDATE marbles_issue SET title=$3,description=$4,priority=$5,status=$6,closed_at=$7,labels_json=$8,parent=$9,available_at=$10,updated_at=$11,evidence_json=$12,metadata_json=$13,external_ref=$14 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&issue.title,&issue.description,&issue.priority,&issue.status,&issue.closed_at,&labels,&issue.parent,&available,&at,&evidence,&metadata,&issue.external_ref])?;
            if status_changed { log_event(tx,&self.company_id,id,at,actor,"status",&format!("→ {}",issue.status))?; } log_event(tx,&self.company_id,id,at,actor,"updated","")
        })?;
        self.get(id)
    }

    fn add_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx| { let row=tx.query_opt("SELECT labels_json FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&id])?.ok_or_else(||Error::NotFound(id.into()))?;let mut labels:Vec<String>=serde_json::from_str(row.get(0))?;if !labels.iter().any(|v|v==label){labels.push(label.into());labels.sort();let encoded=serde_json::to_string(&labels)?;tx.execute("UPDATE marbles_issue SET labels_json=$3,updated_at=$4 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&encoded,&at])?;log_event(tx,&self.company_id,id,at,actor,"labels",&labels.join(","))?;}Ok(()) })
    }
    fn remove_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx| { let row=tx.query_opt("SELECT labels_json FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&id])?.ok_or_else(||Error::NotFound(id.into()))?;let mut labels:Vec<String>=serde_json::from_str(row.get(0))?;labels.retain(|v|v!=label);let encoded=serde_json::to_string(&labels)?;tx.execute("UPDATE marbles_issue SET labels_json=$3,updated_at=$4 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&encoded,&at])?;log_event(tx,&self.company_id,id,at,actor,"labels",&labels.join(",")) })
    }

    fn add_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        if child == parent {
            return Err(Error::Cycle {
                child: child.into(),
                parent: parent.into(),
            });
        }
        self.transaction(|tx| { require_issue(tx,&self.company_id,child)?;require_issue(tx,&self.company_id,parent)?; if reachable(tx,&self.company_id,parent,child)?{return Err(Error::Cycle{child:child.into(),parent:parent.into()});} tx.execute("INSERT INTO marbles_dep(company_id,issue_id,depends_on) VALUES($1,$2,$3) ON CONFLICT DO NOTHING", &[&self.company_id,&child,&parent])?;tx.execute("UPDATE marbles_issue SET updated_at=$3 WHERE company_id=$1 AND id=$2", &[&self.company_id,&child,&at])?;log_event(tx,&self.company_id,child,at,actor,"depends_on",parent) })
    }
    fn remove_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM marbles_dep WHERE company_id=$1 AND issue_id=$2 AND depends_on=$3",
                &[&self.company_id, &child, &parent],
            )?;
            log_event(
                tx,
                &self.company_id,
                child,
                at,
                actor,
                "unblocked_from",
                parent,
            )
        })
    }

    fn claim(&self, req: &ClaimRequest, at: i64) -> Result<ClaimReceipt> {
        let ttl = match req.actor_kind {
            ActorKind::Agent => req
                .ttl_seconds
                .unwrap_or(self.policy.agent_ttl_seconds)
                .min(self.policy.max_ttl_seconds),
            ActorKind::Human => req
                .ttl_seconds
                .map(|s| {
                    self.week
                        .deadline(&utc(at), s.div_euclid(60).max(1))
                        .timestamp()
                        - at
                })
                .unwrap_or(self.week.human_expiry(&utc(at), &self.policy) - at)
                .min(self.policy.max_ttl_seconds),
        };
        self.transaction(|tx| { let row=tx.query_opt("SELECT assignee,expires_at FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&req.id])?.ok_or_else(||Error::NotFound(req.id.clone()))?;let assignee:Option<String>=row.get(0);let expires:Option<i64>=row.get(1);if assignee.is_some()&&expires.is_some_and(|v|v>at)&&assignee.as_deref()!=Some(&req.assignee){return Err(Error::AlreadyClaimed(req.id.clone(),assignee.unwrap()));}let expiry=at+ttl;tx.execute("UPDATE marbles_issue SET assignee=$3,actor_kind=$4,claimed_at=$5,expires_at=$6,updated_at=$5 WHERE company_id=$1 AND id=$2", &[&self.company_id,&req.id,&req.assignee,&req.actor_kind.as_str(),&at,&expiry])?;log_event(tx,&self.company_id,&req.id,at,&req.assignee,"claimed",req.actor_kind.as_str())?;Ok(ClaimReceipt{id:req.id.clone(),assignee:req.assignee.clone(),actor_kind:req.actor_kind,expires_at:expiry}) })
    }
    fn touch(&self, id: &str, assignee: &str, at: i64) -> Result<i64> {
        self.transaction(|tx| { let row=tx.query_opt("SELECT assignee,actor_kind FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&id])?.ok_or_else(||Error::NotFound(id.into()))?;let current:Option<String>=row.get(0);if current.as_deref()!=Some(assignee){return Err(Error::Bad(format!("{id} is not claimed by {assignee}")));}let kind:Option<String>=row.get(1);let ttl=if kind.as_deref()==Some("human"){self.week.human_expiry(&utc(at),&self.policy)-at}else{self.policy.agent_ttl_seconds};let expiry=at+ttl;tx.execute("UPDATE marbles_issue SET expires_at=$3,updated_at=$4 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&expiry,&at])?;log_event(tx,&self.company_id,id,at,assignee,"heartbeat","")?;Ok(expiry) })
    }
    fn release(&self, id: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx|{let changed=tx.execute("UPDATE marbles_issue SET assignee=NULL,actor_kind=NULL,claimed_at=NULL,expires_at=NULL,updated_at=$3 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&at])?;if changed==0{return Err(Error::NotFound(id.into()));}log_event(tx,&self.company_id,id,at,actor,"released","")})
    }

    fn sweep(&self, at: i64) -> Result<SweepReport> {
        self.transaction(|tx| { let rows=tx.query("SELECT id,assignee,actor_kind FROM marbles_issue WHERE company_id=$1 AND assignee IS NOT NULL AND expires_at IS NOT NULL AND expires_at <= $2 FOR UPDATE", &[&self.company_id,&at])?;let mut report=SweepReport::default();for row in rows{let id:String=row.get(0);let assignee:String=row.get(1);let kind:String=row.get(2);if kind=="human"{report.escalated.push(format!("{id} ({assignee})"));log_event(tx,&self.company_id,&id,at,"sweeper","escalated",&assignee)?;}else{tx.execute("UPDATE marbles_issue SET assignee=NULL,actor_kind=NULL,claimed_at=NULL,expires_at=NULL WHERE company_id=$1 AND id=$2", &[&self.company_id,&id])?;log_event(tx,&self.company_id,&id,at,"sweeper","claim_expired",&assignee)?;report.requeued.push(id);}}Ok(report) })
    }

    fn close(
        &self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue> {
        if evidence.is_empty() {
            return Err(Error::NoEvidence(id.into()));
        }
        self.transaction(|tx|{let blockers=tx.query("SELECT d.depends_on FROM marbles_dep d JOIN marbles_issue p ON p.company_id=d.company_id AND p.id=d.depends_on WHERE d.company_id=$1 AND d.issue_id=$2 AND p.status!='closed' FOR UPDATE OF p", &[&self.company_id,&id])?;if let Some(row)=blockers.first(){let blocker:String=row.get(0);return Err(Error::Bad(format!("{id} still waits on {blocker}; close it first or remove the dependency")));}close_tx(tx,&self.company_id,id,reason,evidence,actor,at)})?;
        self.get(id)
    }
    fn supersede(&self, id: &str, replacement: &str, actor: &str, at: i64) -> Result<()> {
        self.transaction(|tx|{require_issue(tx,&self.company_id,replacement)?;let reason=format!("superseded by {replacement}");close_tx(tx,&self.company_id,id,Some(&reason),&[Evidence::ack(reason.clone())],actor,at)?;let row=tx.query_one("SELECT labels_json FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE", &[&self.company_id,&id])?;let mut labels:Vec<String>=serde_json::from_str(row.get(0))?;let label=format!("supersedes:{replacement}");if !labels.contains(&label){labels.push(label);labels.sort();let encoded=serde_json::to_string(&labels)?;tx.execute("UPDATE marbles_issue SET labels_json=$3,updated_at=$4 WHERE company_id=$1 AND id=$2", &[&self.company_id,&id,&encoded,&at])?;log_event(tx,&self.company_id,id,at,actor,"labels",&labels.join(","))?;}Ok(())})
    }
    fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        let mut client = self.client()?;
        Ok(client.query("SELECT issue_id,ts,actor,event,detail FROM marbles_history WHERE company_id=$1 AND issue_id=$2 ORDER BY seq", &[&self.company_id,&id])?.into_iter().map(|r|HistoryEvent{issue_id:r.get(0),ts:r.get(1),actor:r.get(2),event:r.get(3),detail:r.get(4)}).collect())
    }
    fn stats(&self) -> Result<serde_json::Value> {
        let mut client = self.client()?;
        let mut counts: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        for r in client.query("SELECT project,status,COUNT(*) FROM marbles_issue WHERE company_id=$1 GROUP BY project,status", &[&self.company_id])?{counts.entry(r.get(0)).or_default().insert(r.get(1),r.get(2));}
        Ok(serde_json::json!(
            counts
                .into_iter()
                .map(|(project, statuses)| {
                    let total: i64 = statuses.values().sum();
                    (
                        project,
                        serde_json::json!({"by_status":statuses,"total":total}),
                    )
                })
                .collect::<serde_json::Map<_, _>>()
        ))
    }
    fn prometheus_metrics(&self, at: i64) -> Result<String> {
        let mut client = self.client()?;
        let mut out = String::from(
            "# HELP marbles_issues Current issues by project and status.\n# TYPE marbles_issues gauge\n# HELP marbles_queue_oldest_age_seconds Age of the oldest currently ready issue.\n# TYPE marbles_queue_oldest_age_seconds gauge\n# HELP marbles_claims_active Current unexpired claims by project and actor kind.\n# TYPE marbles_claims_active gauge\n# HELP marbles_events_total Durable tracker transitions by project and event.\n# TYPE marbles_events_total counter\n# HELP marbles_claim_latency_seconds Time from issue creation to first claim.\n# TYPE marbles_claim_latency_seconds summary\n# HELP marbles_cycle_time_seconds Time from issue creation to evidence-backed close.\n# TYPE marbles_cycle_time_seconds summary\n# HELP marbles_review_time_seconds Time from first review state to close.\n# TYPE marbles_review_time_seconds summary\n",
        );
        for r in client.query("SELECT project,status,COUNT(*) FROM marbles_issue WHERE company_id=$1 GROUP BY project,status ORDER BY project,status", &[&self.company_id])?{let p:String=r.get(0);let s:String=r.get(1);let n:i64=r.get(2);out.push_str(&format!("marbles_issues{{project=\"{}\",status=\"{}\"}} {n}\n",prometheus_label(&p),prometheus_label(&s)));}
        for r in client.query("SELECT i.project,$2-MIN(i.created_at) FROM marbles_issue i WHERE i.company_id=$1 AND i.status='open' AND COALESCE(i.available_at,0)<=$2 AND NOT EXISTS(SELECT 1 FROM marbles_dep d JOIN marbles_issue p ON p.company_id=d.company_id AND p.id=d.depends_on WHERE d.company_id=i.company_id AND d.issue_id=i.id AND p.status!='closed') GROUP BY i.project", &[&self.company_id,&at])?{let p:String=r.get(0);let age:i64=r.get(1);out.push_str(&format!("marbles_queue_oldest_age_seconds{{project=\"{}\"}} {}\n",prometheus_label(&p),age.max(0)));}
        for r in client.query("SELECT project,actor_kind,COUNT(*) FROM marbles_issue WHERE company_id=$1 AND assignee IS NOT NULL AND expires_at>$2 GROUP BY project,actor_kind", &[&self.company_id,&at])?{let p:String=r.get(0);let k:String=r.get(1);let n:i64=r.get(2);out.push_str(&format!("marbles_claims_active{{project=\"{}\",actor_kind=\"{}\"}} {n}\n",prometheus_label(&p),prometheus_label(&k)));}
        for r in client.query("SELECT i.project,h.event,COUNT(*) FROM marbles_history h JOIN marbles_issue i ON i.company_id=h.company_id AND i.id=h.issue_id WHERE h.company_id=$1 GROUP BY i.project,h.event", &[&self.company_id])?{let p:String=r.get(0);let e:String=r.get(1);let n:i64=r.get(2);out.push_str(&format!("marbles_events_total{{project=\"{}\",event=\"{}\"}} {n}\n",prometheus_label(&p),prometheus_label(&e)));}
        append_duration(
            &mut *client,
            &mut out,
            &self.company_id,
            "marbles_claim_latency_seconds",
            "SELECT i.project,SUM(h.first_claim-i.created_at)::bigint,COUNT(*) FROM marbles_issue i JOIN (SELECT company_id,issue_id,MIN(ts) first_claim FROM marbles_history WHERE company_id=$1 AND event='claimed' GROUP BY company_id,issue_id) h ON h.company_id=i.company_id AND h.issue_id=i.id WHERE i.company_id=$1 AND h.first_claim>=i.created_at GROUP BY i.project",
        )?;
        append_duration(
            &mut *client,
            &mut out,
            &self.company_id,
            "marbles_cycle_time_seconds",
            "SELECT project,SUM(closed_at-created_at)::bigint,COUNT(*) FROM marbles_issue WHERE company_id=$1 AND closed_at IS NOT NULL AND closed_at>=created_at GROUP BY project",
        )?;
        append_duration(
            &mut *client,
            &mut out,
            &self.company_id,
            "marbles_review_time_seconds",
            "SELECT i.project,SUM(i.closed_at-h.first_review)::bigint,COUNT(*) FROM marbles_issue i JOIN (SELECT company_id,issue_id,MIN(ts) first_review FROM marbles_history WHERE company_id=$1 AND event='status' AND detail='→ review' GROUP BY company_id,issue_id) h ON h.company_id=i.company_id AND h.issue_id=i.id WHERE i.company_id=$1 AND i.closed_at IS NOT NULL AND i.closed_at>=h.first_review GROUP BY i.project",
        )?;
        Ok(out)
    }
}

fn row_to_issue(client: &mut impl GenericClient, company: &str, id: &str) -> Result<Option<Issue>> {
    let Some(r)=client.query_opt("SELECT id,title,description,status,issue_type,priority,parent,labels_json,assignee,actor_kind,expires_at,available_at,created_at,updated_at,project,closed_at,metadata_json,external_ref,evidence_json FROM marbles_issue WHERE company_id=$1 AND id=$2", &[&company,&id])? else{return Ok(None)};
    let deps=client.query("SELECT depends_on FROM marbles_dep WHERE company_id=$1 AND issue_id=$2 ORDER BY depends_on", &[&company,&id])?.into_iter().map(|r|Dependency{depends_on_id:Some(r.get(0))}).collect();
    let dependency_count:i64=client.query_one("SELECT COUNT(*) FROM marbles_dep d JOIN marbles_issue p ON p.company_id=d.company_id AND p.id=d.depends_on WHERE d.company_id=$1 AND d.issue_id=$2 AND p.status!='closed'", &[&company,&id])?.get(0);
    let dependent_count:i64=client.query_one("SELECT COUNT(*) FROM marbles_dep d JOIN marbles_issue c ON c.company_id=d.company_id AND c.id=d.issue_id WHERE d.company_id=$1 AND d.depends_on=$2 AND c.status!='closed'", &[&company,&id])?.get(0);
    Ok(Some(Issue {
        id: r.get(0),
        title: r.get(1),
        description: r.get(2),
        status: r.get(3),
        issue_type: r.get(4),
        priority: r.get(5),
        parent: r.get(6),
        labels: serde_json::from_str(r.get(7))?,
        assignee: r.get(8),
        actor_kind: r.get(9),
        expires_at: r.get(10),
        available_at: r.get(11),
        created_at: r.get(12),
        updated_at: r.get(13),
        project: r.get(14),
        closed_at: r.get(15),
        metadata: serde_json::from_str(r.get(16))?,
        external_ref: r.get(17),
        evidence: serde_json::from_str(r.get(18))?,
        dependencies: deps,
        dependency_count,
        dependent_count,
    }))
}
fn require_issue(client: &mut impl GenericClient, company: &str, id: &str) -> Result<()> {
    if client
        .query_opt(
            "SELECT 1 FROM marbles_issue WHERE company_id=$1 AND id=$2",
            &[&company, &id],
        )?
        .is_none()
    {
        Err(Error::NotFound(id.into()))
    } else {
        Ok(())
    }
}
fn log_event(
    tx: &mut Transaction<'_>,
    company: &str,
    id: &str,
    ts: i64,
    actor: &str,
    event: &str,
    detail: &str,
) -> Result<()> {
    let seq: i64 = tx
        .query_one(
            "SELECT COALESCE(MAX(seq),0)+1 FROM marbles_history WHERE company_id=$1",
            &[&company],
        )?
        .get(0);
    tx.execute("INSERT INTO marbles_history(company_id,seq,issue_id,ts,actor,event,detail) VALUES($1,$2,$3,$4,$5,$6,$7)", &[&company,&seq,&id,&ts,&actor,&event,&detail])?;
    tx.execute("INSERT INTO marbles_outbound_event(company_id,seq,issue_id,project,occurred_at,event,next_attempt_at) SELECT $1,$2,id,project,$4,$5,$4 FROM marbles_issue WHERE company_id=$1 AND id=$3", &[&company,&seq,&id,&ts,&event])?;
    Ok(())
}
fn close_tx(
    tx: &mut Transaction<'_>,
    company: &str,
    id: &str,
    reason: Option<&str>,
    evidence: &[Evidence],
    actor: &str,
    at: i64,
) -> Result<()> {
    let row = tx
        .query_opt(
            "SELECT evidence_json FROM marbles_issue WHERE company_id=$1 AND id=$2 FOR UPDATE",
            &[&company, &id],
        )?
        .ok_or_else(|| Error::NotFound(id.into()))?;
    let mut merged: Vec<Evidence> = serde_json::from_str(row.get(0))?;
    for item in evidence {
        if !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    let encoded = serde_json::to_string(&merged)?;
    tx.execute("UPDATE marbles_issue SET status='closed',closed_at=$3,close_reason=$4,evidence_json=$5,assignee=NULL,actor_kind=NULL,claimed_at=NULL,expires_at=NULL,updated_at=$3 WHERE company_id=$1 AND id=$2", &[&company,&id,&at,&reason,&encoded])?;
    let detail = format!(
        "{} | {}",
        reason.unwrap_or(""),
        evidence
            .iter()
            .map(|e| format!("{}:{}", e.kind, e.value))
            .collect::<Vec<_>>()
            .join(" ")
    );
    log_event(tx, company, id, at, actor, "closed", &detail)
}
fn reachable(
    client: &mut impl GenericClient,
    company: &str,
    from: &str,
    target: &str,
) -> Result<bool> {
    Ok(client.query_opt("WITH RECURSIVE walk(id) AS (SELECT $2::text UNION SELECT d.depends_on FROM marbles_dep d JOIN walk w ON d.issue_id=w.id WHERE d.company_id=$1) SELECT 1 FROM walk WHERE id=$3 LIMIT 1", &[&company,&from,&target])?.is_some())
}
fn short_suffix() -> String {
    const A: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("rng");
    bytes
        .iter()
        .map(|b| A[*b as usize % A.len()] as char)
        .collect()
}
fn utc(secs: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().expect("valid epoch")
}
fn prometheus_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}
fn append_duration(
    client: &mut impl GenericClient,
    out: &mut String,
    company: &str,
    metric: &str,
    sql: &str,
) -> Result<()> {
    for row in client.query(sql, &[&company])? {
        let project: String = row.get(0);
        let sum: i64 = row.get(1);
        let count: i64 = row.get(2);
        let project = prometheus_label(&project);
        out.push_str(&format!(
            "{metric}_sum{{project=\"{project}\"}} {}\n",
            sum.max(0)
        ));
        out.push_str(&format!(
            "{metric}_count{{project=\"{project}\"}} {count}\n"
        ));
    }
    Ok(())
}
