//! Dated facts on entities. Two timelines (graphiti): `valid_from..valid_to` = when it was true,
//! `recorded_at` = when we learned it. Writes follow mem0: same current value → NOOP, different →
//! insert and supersede (old row keeps its history; never deleted).

use super::slugify;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Fact {
    pub id: Uuid,
    pub entity: String,
    pub entity_name: String,
    pub attribute: String,
    pub value_num: Option<f64>,
    pub value_hi: Option<f64>,
    pub unit: Option<String>,
    pub value_text: Option<String>,
    pub valid_from: Option<NaiveDate>,
    pub valid_to: Option<NaiveDate>,
    pub recorded_at: DateTime<Utc>,
    pub source_uri: Option<String>,
    pub source_title: Option<String>,
    pub confidence: Option<f32>,
    pub verified_by: Option<String>,
    pub superseded_by: Option<Uuid>,
    pub note: Option<String>,
}

impl Fact {
    /// `273 GB/s`, `1.2–1.5 USD`, or the text value.
    pub fn value_display(&self) -> String {
        let num = |x: f64| {
            if x.fract() == 0.0 && x.abs() < 1e15 { format!("{}", x as i64) } else { format!("{x}") }
        };
        let mut s = match (self.value_num, self.value_hi) {
            (Some(a), Some(b)) => format!("{}–{}", num(a), num(b)),
            (Some(a), None) => num(a),
            _ => String::new(),
        };
        if let Some(u) = &self.unit
            && !s.is_empty()
        {
            s.push(' ');
            s.push_str(u);
        }
        if let Some(t) = &self.value_text {
            if s.is_empty() {
                s = t.clone();
            } else {
                s = format!("{s} ({t})");
            }
        }
        s
    }

    /// One markdown bullet: `- attribute: value (valid 2026-01-01..; source)`.
    pub fn to_markdown(&self) -> String {
        let mut s = format!("- {}: {}", self.attribute, self.value_display());
        let mut meta = Vec::new();
        if self.valid_from.is_some() || self.valid_to.is_some() {
            meta.push(format!(
                "valid {}..{}",
                self.valid_from.map(|d| d.to_string()).unwrap_or_default(),
                self.valid_to.map(|d| d.to_string()).unwrap_or_default()
            ));
        }
        if let Some(v) = &self.verified_by {
            meta.push(format!("verified: {v}"));
        }
        if let Some(src) = self.source_title.as_ref().or(self.source_uri.as_ref()) {
            meta.push(format!("source: {src}"));
        }
        if self.superseded_by.is_some() {
            meta.push("superseded".into());
        }
        if !meta.is_empty() {
            s.push_str(&format!(" ({})", meta.join("; ")));
        }
        s
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NewFact {
    /// Entity slug, name or alias.
    pub entity: String,
    /// Used only when the entity does not exist yet (chip, model, engine, provider, ...).
    #[serde(default)]
    pub entity_kind: Option<String>,
    pub attribute: String,
    #[serde(default)]
    pub value_num: Option<f64>,
    #[serde(default)]
    pub value_hi: Option<f64>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub value_text: Option<String>,
    #[serde(default)]
    pub valid_from: Option<NaiveDate>,
    #[serde(default)]
    pub valid_to: Option<NaiveDate>,
    #[serde(default)]
    pub source_uri: Option<String>,
    #[serde(default)]
    pub source_title: Option<String>,
    #[serde(default)]
    pub confidence: Option<f32>,
    /// owner | measured | vendor | community (owner = the configured owner's name)
    #[serde(default)]
    pub verified_by: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FactAction {
    Added,
    Updated,
    Noop,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FactOutcome {
    pub action: FactAction,
    /// The new fact, or the existing one on NOOP.
    pub id: Uuid,
    /// The fact this one replaced (UPDATE only).
    pub superseded: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub(crate) struct EntityRow {
    pub id: Uuid,
    pub slug: String,
    pub doc_id: Option<Uuid>,
}

/// Find an entity by slug, name or alias (case-insensitive), or by the slug of `name`.
pub(crate) async fn find_entity<'e, E>(exec: E, name: &str) -> Result<Option<EntityRow>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let name = name.trim();
    let name = name.strip_prefix("/docs/entity/").unwrap_or(name);
    let row = sqlx::query(
        "select id, slug, doc_id from kb.entities
         where lower(slug) = lower($1) or slug = $2 or lower(name) = lower($1)
            or exists (select 1 from unnest(aliases) a where lower(a) = lower($1))
         order by (lower(slug) = lower($1)) desc, (slug = $2) desc, (lower(name) = lower($1)) desc, slug
         limit 1",
    )
    .bind(name)
    .bind(slugify(name))
    .fetch_optional(exec)
    .await?;
    Ok(row.map(|r| EntityRow { id: r.get("id"), slug: r.get("slug"), doc_id: r.get("doc_id") }))
}

const FACT_COLS: &str = "f.id, e.slug as entity, e.name as entity_name, f.attribute, f.value_num::float8 as value_num,
    f.value_hi::float8 as value_hi, f.unit, f.value_text, f.valid_from, f.valid_to, f.recorded_at,
    s.uri as source_uri, s.title as source_title, f.confidence, f.verified_by, f.superseded_by, f.note";

fn fact_from_row(r: &sqlx::postgres::PgRow) -> Fact {
    Fact {
        id: r.get("id"),
        entity: r.get("entity"),
        entity_name: r.get("entity_name"),
        attribute: r.get("attribute"),
        value_num: r.get("value_num"),
        value_hi: r.get("value_hi"),
        unit: r.get("unit"),
        value_text: r.get("value_text"),
        valid_from: r.get("valid_from"),
        valid_to: r.get("valid_to"),
        recorded_at: r.get("recorded_at"),
        source_uri: r.get("source_uri"),
        source_title: r.get("source_title"),
        confidence: r.get("confidence"),
        verified_by: r.get("verified_by"),
        superseded_by: r.get("superseded_by"),
        note: r.get("note"),
    }
}

/// Facts about an entity (slug, name or alias). Default: current facts (not superseded).
/// `as_of`: the fact per attribute whose validity covers that date (superseded rows included, so
/// past values come back). `include_history`: every row, newest first. Unknown entity → empty.
pub async fn facts(
    pool: &PgPool,
    entity: &str,
    attribute: Option<&str>,
    as_of: Option<NaiveDate>,
    include_history: bool,
) -> Result<Vec<Fact>> {
    let Some(ent) = find_entity(pool, entity).await? else { return Ok(vec![]) };
    let covers = "($3::date is null or ((f.valid_from is null or f.valid_from <= $3) and (f.valid_to is null or f.valid_to >= $3)))";
    let sql = if include_history {
        format!(
            "select {FACT_COLS} from kb.facts f join kb.entities e on e.id = f.entity_id
             left join kb.sources s on s.id = f.source_id
             where f.entity_id = $1 and ($2::text is null or f.attribute = $2) and {covers}
             order by f.attribute, f.recorded_at desc"
        )
    } else if as_of.is_some() {
        format!(
            "select * from (
               select distinct on (f.attribute) {FACT_COLS} from kb.facts f join kb.entities e on e.id = f.entity_id
               left join kb.sources s on s.id = f.source_id
               where f.entity_id = $1 and ($2::text is null or f.attribute = $2) and {covers}
               order by f.attribute, (f.superseded_by is null) desc, f.valid_from desc nulls last, f.recorded_at desc
             ) x order by attribute"
        )
    } else {
        format!(
            "select {FACT_COLS} from kb.facts f join kb.entities e on e.id = f.entity_id
             left join kb.sources s on s.id = f.source_id
             where f.entity_id = $1 and ($2::text is null or f.attribute = $2) and f.superseded_by is null and {covers}
             order by f.attribute, f.recorded_at desc"
        )
    };
    let rows = sqlx::query(&sql).bind(ent.id).bind(attribute).bind(as_of).fetch_all(pool).await?;
    Ok(rows.iter().map(fact_from_row).collect())
}

/// Record a fact: NOOP if the current value for (entity, attribute) is identical, otherwise insert
/// it and supersede the current row(s). Creates the entity (needs `entity_kind`) and the source
/// (deduped by URI) when missing.
pub async fn add_fact(pool: &PgPool, f: NewFact) -> Result<FactOutcome> {
    let attribute = f.attribute.trim();
    if attribute.is_empty() {
        bail!("attribute is required");
    }
    if f.value_num.is_none() && f.value_text.as_deref().is_none_or(|t| t.trim().is_empty()) {
        bail!("a fact needs value_num or value_text");
    }
    if let Some(c) = f.confidence
        && !(0.0..=1.0).contains(&c)
    {
        bail!("confidence must be between 0 and 1");
    }
    if let (Some(a), Some(b)) = (f.valid_from, f.valid_to)
        && b < a
    {
        bail!("valid_to is before valid_from");
    }

    let mut tx = pool.begin().await?;
    let entity_id = match find_entity(&mut *tx, &f.entity).await? {
        Some(e) => e.id,
        None => {
            let kind = f
                .entity_kind
                .as_deref()
                .filter(|k| !k.trim().is_empty())
                .with_context(|| format!("entity {:?} not found; pass entity_kind to create it", f.entity))?;
            let slug = slugify(&f.entity);
            if slug.is_empty() {
                bail!("entity name {:?} has no usable characters", f.entity);
            }
            sqlx::query_scalar(
                "insert into kb.entities (slug, kind, name) values ($1, $2, $3)
                 on conflict (slug) do update set slug = excluded.slug returning id",
            )
            .bind(&slug)
            .bind(kind.trim())
            .bind(f.entity.trim())
            .fetch_one(&mut *tx)
            .await?
        }
    };
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1::uuid::text || '|' || $2, 0))")
        .bind(entity_id)
        .bind(attribute)
        .execute(&mut *tx)
        .await?;

    let current: Vec<(Uuid, bool)> = sqlx::query_as(
        "select id,
                (value_num is not distinct from $3::float8::numeric and value_hi is not distinct from $4::float8::numeric
                 and unit is not distinct from $5 and value_text is not distinct from $6) as same
         from kb.facts where entity_id = $1 and attribute = $2 and superseded_by is null
         order by recorded_at desc",
    )
    .bind(entity_id)
    .bind(attribute)
    .bind(f.value_num)
    .bind(f.value_hi)
    .bind(&f.unit)
    .bind(&f.value_text)
    .fetch_all(&mut *tx)
    .await?;
    if let Some((id, true)) = current.first() {
        tx.commit().await?;
        return Ok(FactOutcome { action: FactAction::Noop, id: *id, superseded: None });
    }

    let source_id: Option<Uuid> = match f.source_uri.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        Some(uri) => {
            let existing: Option<Uuid> =
                sqlx::query_scalar("select id from kb.sources where uri = $1 order by fetched_at limit 1")
                    .bind(uri)
                    .fetch_optional(&mut *tx)
                    .await?;
            match existing {
                Some(id) => Some(id),
                None => Some(
                    sqlx::query_scalar("insert into kb.sources (kind, uri, title) values ($1, $2, $3) returning id")
                        .bind(if uri.contains("://") { "url" } else { "note" })
                        .bind(uri)
                        .bind(&f.source_title)
                        .fetch_one(&mut *tx)
                        .await?,
                ),
            }
        }
        None => None,
    };

    let id: Uuid = sqlx::query_scalar(
        "insert into kb.facts (entity_id, attribute, value_num, value_hi, unit, value_text, valid_from, valid_to,
                               source_id, confidence, verified_by, note)
         values ($1, $2, $3::float8::numeric, $4::float8::numeric, $5, $6, $7, $8, $9, $10, $11, $12)
         returning id",
    )
    .bind(entity_id)
    .bind(attribute)
    .bind(f.value_num)
    .bind(f.value_hi)
    .bind(&f.unit)
    .bind(&f.value_text)
    .bind(f.valid_from)
    .bind(f.valid_to)
    .bind(source_id)
    .bind(f.confidence)
    .bind(&f.verified_by)
    .bind(&f.note)
    .fetch_one(&mut *tx)
    .await?;

    // Old rows: point at the new one; if the new value starts later, close the old validity window.
    sqlx::query(
        "update kb.facts set superseded_by = $3,
           valid_to = case when valid_to is null and $4::date is not null and (valid_from is null or valid_from < $4)
                           then $4::date - 1 else valid_to end
         where entity_id = $1 and attribute = $2 and superseded_by is null and id <> $3",
    )
    .bind(entity_id)
    .bind(attribute)
    .bind(id)
    .bind(f.valid_from)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let superseded = current.first().map(|(id, _)| *id);
    Ok(FactOutcome {
        action: if superseded.is_some() { FactAction::Updated } else { FactAction::Added },
        id,
        superseded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_display() {
        let mut f = Fact {
            id: Uuid::nil(),
            entity: "gb10".into(),
            entity_name: "GB10".into(),
            attribute: "memory_bandwidth".into(),
            value_num: Some(273.0),
            value_hi: None,
            unit: Some("GB/s".into()),
            value_text: None,
            valid_from: None,
            valid_to: None,
            recorded_at: Utc::now(),
            source_uri: None,
            source_title: None,
            confidence: None,
            verified_by: None,
            superseded_by: None,
            note: None,
        };
        assert_eq!(f.value_display(), "273 GB/s");
        f.value_hi = Some(301.5);
        assert_eq!(f.value_display(), "273–301.5 GB/s");
        assert!(f.to_markdown().starts_with("- memory_bandwidth: 273–301.5 GB/s"));
    }
}
