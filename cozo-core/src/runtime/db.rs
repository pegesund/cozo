/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::BTreeMap;
use std::default::Default;
use std::fmt::{Debug, Formatter};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use either::{Left, Right};
use itertools::Itertools;
use miette::{bail, ensure, miette, Diagnostic, IntoDiagnostic, Result, WrapErr};
use serde_json::{json, Map};
use smartstring::SmartString;
use thiserror::Error;

use crate::data::json::JsonValue;
use crate::data::program::{InputProgram, QueryAssertion, RelationOp};
use crate::data::relation::ColumnDef;
use crate::data::tuple::Tuple;
use crate::data::value::{DataValue, LARGEST_UTF_CHAR};
use crate::decode_tuple_from_kv;
use crate::parse::sys::SysOp;
use crate::parse::{parse_script, CozoScript, SourceSpan};
use crate::query::compile::{CompiledProgram, CompiledRule, CompiledRuleSet};
use crate::query::relation::{
    FilteredRA, InMemRelationRA, InnerJoin, NegJoin, RelAlgebra, ReorderRA, StoredRA, UnificationRA,
};
use crate::runtime::relation::{RelationHandle, RelationId};
use crate::runtime::transact::SessionTx;
use crate::storage::{Storage, StoreTx};

struct RunningQueryHandle {
    started_at: f64,
    poison: Poison,
}

struct RunningQueryCleanup {
    id: u64,
    running_queries: Arc<Mutex<BTreeMap<u64, RunningQueryHandle>>>,
}

impl Drop for RunningQueryCleanup {
    fn drop(&mut self) {
        let mut map = self.running_queries.lock().unwrap();
        if let Some(handle) = map.remove(&self.id) {
            handle.poison.0.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(serde_derive::Serialize, serde_derive::Deserialize)]
pub struct DbManifest {
    pub storage_version: u64,
}

/// The database object of Cozo.
#[derive(Clone)]
pub struct Db<S> {
    db: S,
    relation_store_id: Arc<AtomicU64>,
    queries_count: Arc<AtomicU64>,
    running_queries: Arc<Mutex<BTreeMap<u64, RunningQueryHandle>>>,
}

impl<S> Debug for Db<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Db")
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("Initialization of database failed")]
#[diagnostic(code(db::init))]
pub(crate) struct BadDbInit(#[help] pub(crate) String);

impl<'s, S: Storage<'s>> Db<S> {
    /// Create a new database object with the given storage.
    /// You must call [`initialize`](Self::initialize) immediately after creation.
    /// Due to lifetime restrictions we are not able to call that for you automatically.
    pub fn new(storage: S) -> Result<Self> {
        let ret = Self {
            db: storage,
            relation_store_id: Arc::new(Default::default()),
            queries_count: Arc::new(Default::default()),
            running_queries: Arc::new(Mutex::new(Default::default())),
        };
        Ok(ret)
    }

    /// Must be called after creation of the database to initialize the runtime state.
    pub fn initialize(&'s self) -> Result<()> {
        self.load_last_ids()?;
        Ok(())
    }

    /// Run the CozoScript passed in. The `params` argument is a map of parameters.
    pub fn run_script(
        &'s self,
        payload: &str,
        params: BTreeMap<String, JsonValue>,
    ) -> Result<JsonValue> {
        #[cfg(not(feature = "wasm"))]
        let start = Instant::now();

        let params = params.into_iter().map(|(k, v)| (k, DataValue::from(v))).collect();
        match self.do_run_script(payload, &params) {
            Ok(mut json) => {
                {
                    #[cfg(not(feature = "wasm"))]
                    let took = start.elapsed().as_secs_f64();
                    let map = json.as_object_mut().unwrap();
                    map.insert("ok".to_string(), json!(true));
                    #[cfg(not(feature = "wasm"))]
                    map.insert("took".to_string(), json!(took));
                }
                Ok(json)
            }
            err => err,
        }
    }
    /// Export relations to JSON data.
    ///
    /// `relations` contains names of the stored relations to export.
    ///
    /// If `as_objects` is `true`, then the output contains objects (maps) for each row,
    /// otherwise the output contains arrays for each row, with headers attached separately.
    pub fn export_relations<'a>(
        &'s self,
        relations: impl Iterator<Item = &'a str>,
        as_objects: bool,
    ) -> Result<JsonValue> {
        let tx = self.transact()?;
        let mut ret = serde_json::Map::default();
        for rel in relations {
            let handle = tx.get_relation(rel, false)?;
            let mut cols = handle
                .metadata
                .keys
                .iter()
                .map(|col| col.name.clone())
                .collect_vec();
            cols.extend(
                handle
                    .metadata
                    .non_keys
                    .iter()
                    .map(|col| col.name.clone())
                    .collect_vec(),
            );

            let start = Tuple::default().encode_as_key(handle.id);
            let end = Tuple::default().encode_as_key(handle.id.next());
            if as_objects {
                let mut coll = vec![];

                for data in tx.tx.range_scan(&start, &end) {
                    let (k, v) = data?;
                    let tuple = decode_tuple_from_kv(&k, &v);
                    let mut j_map = serde_json::Map::default();
                    for (k, v) in cols.iter().zip(tuple.0) {
                        let j_v = JsonValue::from(v);
                        j_map.insert(k.to_string(), j_v);
                    }
                    coll.push(JsonValue::Object(j_map));
                }
                ret.insert(rel.to_string(), JsonValue::Array(coll));
            } else {
                let mut rows = vec![];
                for data in tx.tx.range_scan(&start, &end) {
                    let (k, v) = data?;
                    let tuple = decode_tuple_from_kv(&k, &v);
                    let row = tuple
                        .0
                        .into_iter()
                        .map(|dv| JsonValue::from(dv))
                        .collect_vec();
                    rows.push(JsonValue::Array(row));
                }
                let headers = cols.iter().map(|col| json!(col)).collect_vec();
                ret.insert(
                    rel.to_string(),
                    json!({
                        "headers": headers,
                        "rows": rows
                    }),
                );
            }
        }
        Ok(JsonValue::Object(ret))
    }
    /// Import relations
    pub fn import_relations(&'s self, data: &Map<String, JsonValue>) -> Result<()> {
        #[derive(Debug, Diagnostic, Error)]
        #[error("cannot import data for relation '{0}': {1}")]
        #[diagnostic(code(import::bad_data))]
        struct BadDataForRelation(String, JsonValue);

        let mut tx = self.transact_write()?;

        for (relation_op, in_data) in data {
            let is_delete;
            let relation: &str = match relation_op.strip_prefix('-') {
                None => {
                    is_delete = false;
                    relation_op
                }
                Some(s) => {
                    is_delete = true;
                    s
                }
            };
            let handle = tx.get_relation(relation, true)?;
            match in_data {
                JsonValue::Array(in_data) => {
                    for val in in_data {
                        let proc_col = |col: &ColumnDef| {
                            let d_val = match val.get(&col.name as &str) {
                                None => match &col.default_gen {
                                    Some(gen) => gen.clone().eval_to_const()?,
                                    None => {
                                        bail!(BadDataForRelation(relation.to_string(), val.clone()))
                                    }
                                },
                                Some(data) => DataValue::from(data),
                            };
                            col.typing.coerce(d_val)
                        };
                        let keys: Vec<_> =
                            handle.metadata.keys.iter().map(proc_col).try_collect()?;
                        let k_store =
                            handle.encode_key_for_store(&Tuple(keys), Default::default())?;

                        if is_delete {
                            tx.tx.del(&k_store)?;
                        } else {
                            let vals: Vec<_> = handle
                                .metadata
                                .non_keys
                                .iter()
                                .map(proc_col)
                                .try_collect()?;
                            let v_store = handle
                                .encode_val_only_for_store(&Tuple(vals), Default::default())?;
                            tx.tx.put(&k_store, &v_store)?;
                        }
                    }
                }
                JsonValue::Object(map) => {
                    let headers = map.get("headers").ok_or_else(|| {
                        miette!("field 'headers' required for relation {}", relation)
                    })?;
                    let headers = headers.as_array().ok_or_else(|| {
                        miette!("field 'headers' for relation {} must be an array", relation)
                    })?;
                    let header2idx: BTreeMap<_, _> = headers
                        .iter()
                        .enumerate()
                        .map(|(i, k)| -> Result<(&str, usize)> {
                            Ok((
                                k.as_str()
                                    .ok_or_else(|| miette!("header must be strings, got {}", k))?,
                                i,
                            ))
                        })
                        .try_collect()?;

                    let key_indices: Vec<_> = handle
                        .metadata
                        .keys
                        .iter()
                        .map(|col| -> Result<(usize, &ColumnDef)> {
                            let idx = header2idx.get(&col.name as &str).ok_or_else(|| {
                                miette!(
                                    "required header {} not found for relation {}",
                                    col.name,
                                    relation
                                )
                            })?;
                            Ok((*idx, col))
                        })
                        .try_collect()?;

                    let val_indices: Vec<_> = if is_delete {
                        vec![]
                    } else {
                        handle
                            .metadata
                            .non_keys
                            .iter()
                            .map(|col| -> Result<(usize, &ColumnDef)> {
                                let idx = header2idx.get(&col.name as &str).ok_or_else(|| {
                                    miette!(
                                        "required header {} not found for relation {}",
                                        col.name,
                                        relation
                                    )
                                })?;
                                Ok((*idx, col))
                            })
                            .try_collect()?
                    };

                    let rows = map.get("rows").ok_or_else(|| {
                        miette!("field 'rows' required for relation {}", relation)
                    })?;
                    let rows = rows.as_array().ok_or_else(|| {
                        miette!("field 'rows' for relation {} must be an array", relation)
                    })?;

                    for row in rows {
                        let row = row
                            .as_array()
                            .ok_or_else(|| miette!("expect rows to be an array of arrays"))?;
                        let keys: Vec<_> = key_indices
                            .iter()
                            .map(|(i, col)| -> Result<DataValue> {
                                let v = row
                                    .get(*i)
                                    .ok_or_else(|| miette!("row too short: {:?}", row))?;
                                col.typing.coerce(DataValue::from(v))
                            })
                            .try_collect()?;
                        let k_store =
                            handle.encode_key_for_store(&Tuple(keys), Default::default())?;
                        if is_delete {
                            tx.tx.del(&k_store)?;
                        } else {
                            let vals: Vec<_> = val_indices
                                .iter()
                                .map(|(i, col)| -> Result<DataValue> {
                                    let v = row
                                        .get(*i)
                                        .ok_or_else(|| miette!("row too short: {:?}", row))?;
                                    col.typing.coerce(DataValue::from(v))
                                })
                                .try_collect()?;
                            let v_store = handle
                                .encode_val_only_for_store(&Tuple(vals), Default::default())?;
                            tx.tx.put(&k_store, &v_store)?;
                        }
                    }
                }
                _ => bail!("expect import relation to be an array or an object"),
            }
        }
        tx.commit_tx()?;
        Ok(())
    }
    /// Backup the running database into an Sqlite file
    pub fn backup_db(&'s self, out_file: String) -> Result<()> {
        #[cfg(feature = "storage-sqlite")]
        {
            let sqlite_db = crate::new_cozo_sqlite(out_file)?;
            let mut s_tx = sqlite_db.transact_write()?;
            let tx = self.transact()?;
            let iter = tx.tx.range_scan(&[], &[1]);
            s_tx.tx.batch_put(iter)?;
            Ok(())
        }
        #[cfg(not(feature = "storage-sqlite"))]
        bail!("backup requires the 'storage-sqlite' feature to be enabled")
    }
    /// Restore from an Sqlite backup
    pub fn restore_backup(&'s self, in_file: &str) -> Result<()> {
        #[cfg(feature = "storage-sqlite")]
        {
            let sqlite_db = crate::new_cozo_sqlite(in_file.to_string())?;
            let s_tx = sqlite_db.transact_write()?;
            let store_id = s_tx.relation_store_id.load(Ordering::SeqCst);
            if store_id != 0 {
                bail!(
                    "Cannot restore backup: data exists in the current database. \
                You can only restore into a new database."
                );
            }
            let mut tx = self.transact()?;
            let iter = s_tx.tx.range_scan(&[], &[1]);
            tx.tx.batch_put(iter)?;
            Ok(())
        }
        #[cfg(not(feature = "storage-sqlite"))]
        bail!("backup requires the 'storage-sqlite' feature to be enabled")
    }
    /// Import data from relations in a backup file
    pub fn import_from_backup(&'s self, in_file: &str, relations: &[String]) -> Result<()> {
        #[cfg(not(feature = "storage-sqlite"))]
        bail!("backup requires the 'storage-sqlite' feature to be enabled");

        #[cfg(feature = "storage-sqlite")]
        {
            let source_db = crate::new_cozo_sqlite(in_file.to_string())?;
            let mut src_tx = source_db.transact()?;
            let mut dst_tx = self.transact_write()?;

            for relation in relations {
                let src_handle = src_tx.get_relation(relation, false)?;
                let dst_handle = dst_tx.get_relation(relation, true)?;

                let src_lower = Tuple::default().encode_as_key(src_handle.id);
                let src_upper = Tuple::default().encode_as_key(src_handle.id.next());

                let data_it = src_tx.tx.range_scan(&src_lower, &src_upper).map(
                    |src_pair| -> Result<(Vec<u8>, Vec<u8>)> {
                        let (mut src_k, mut src_v) = src_pair?;
                        dst_handle.amend_key_prefix(&mut src_k);
                        dst_handle.amend_key_prefix(&mut src_v);
                        Ok((src_k, src_v))
                    },
                );
                dst_tx.tx.batch_put(Box::new(data_it))?;
            }

            src_tx.commit_tx()?;
            dst_tx.commit_tx()
        }
    }

    fn compact_relation(&'s self) -> Result<()> {
        let l = Tuple::default().encode_as_key(RelationId(0));
        let u = Tuple(vec![DataValue::Bot]).encode_as_key(RelationId(u64::MAX));
        self.db.range_compact(&l, &u)?;
        Ok(())
    }

    fn load_last_ids(&'s self) -> Result<()> {
        let mut tx = self.transact()?;
        self.relation_store_id
            .store(tx.load_last_relation_store_id()?.0, Ordering::Release);
        tx.commit_tx()?;
        Ok(())
    }
    fn transact(&'s self) -> Result<SessionTx<'_>> {
        let ret = SessionTx {
            tx: Box::new(self.db.transact(false)?),
            mem_store_id: Default::default(),
            relation_store_id: self.relation_store_id.clone(),
        };
        Ok(ret)
    }
    fn transact_write(&'s self) -> Result<SessionTx<'_>> {
        let ret = SessionTx {
            tx: Box::new(self.db.transact(true)?),
            mem_store_id: Default::default(),
            relation_store_id: self.relation_store_id.clone(),
        };
        Ok(ret)
    }
    fn do_run_script(
        &'s self,
        payload: &str,
        param_pool: &BTreeMap<String, DataValue>,
    ) -> Result<JsonValue> {
        match parse_script(payload, param_pool)? {
            CozoScript::Multi(ps) => {
                let is_write = ps.iter().any(|p| p.out_opts.store_relation.is_some());
                let mut cleanups = vec![];
                let mut res = json!(null);
                {
                    let mut tx = if is_write {
                        self.transact_write()?
                    } else {
                        self.transact()?
                    };

                    for p in ps {
                        let sleep_opt = p.out_opts.sleep;
                        let (q_res, q_cleanups) = self.run_query(&mut tx, p)?;
                        res = q_res;
                        cleanups.extend(q_cleanups);
                        #[cfg(not(feature = "wasm"))]
                        if let Some(secs) = sleep_opt {
                            thread::sleep(Duration::from_micros((secs * 1000000.) as u64));
                        }
                    }
                    if is_write {
                        tx.commit_tx()?;
                    } else {
                        tx.commit_tx()?;
                        assert!(cleanups.is_empty(), "non-empty cleanups on read-only tx");
                    }
                }

                for (lower, upper) in cleanups {
                    self.db.del_range(&lower, &upper)?;
                }
                Ok(res)
            }
            CozoScript::Sys(op) => self.run_sys_op(op),
        }
    }
    fn explain_compiled(&self, strata: &[CompiledProgram]) -> Result<JsonValue> {
        let mut ret: Vec<JsonValue> = vec![];
        const STRATUM: &str = "stratum";
        const ATOM_IDX: &str = "atom_idx";
        const OP: &str = "op";
        const RULE_IDX: &str = "rule_idx";
        const RULE_NAME: &str = "rule";
        const REF_NAME: &str = "ref";
        const OUT_BINDINGS: &str = "out_relation";
        const JOINS_ON: &str = "joins_on";
        const FILTERS: &str = "filters/expr";

        let headers = [
            STRATUM,
            RULE_IDX,
            RULE_NAME,
            ATOM_IDX,
            OP,
            REF_NAME,
            JOINS_ON,
            FILTERS,
            OUT_BINDINGS,
        ];

        for (stratum, p) in strata.iter().enumerate() {
            let mut clause_idx = -1;
            for (rule_name, v) in p {
                match v {
                    CompiledRuleSet::Rules(rules) => {
                        for CompiledRule { aggr, relation, .. } in rules.iter() {
                            clause_idx += 1;
                            let mut ret_for_relation = vec![];
                            let mut rel_stack = vec![relation];
                            let mut idx = 0;
                            let mut atom_type = "out";
                            for (a, _) in aggr.iter().flatten() {
                                if a.is_meet {
                                    if atom_type == "out" {
                                        atom_type = "meet_aggr_out";
                                    }
                                } else {
                                    atom_type = "aggr_out";
                                }
                            }

                            ret_for_relation.push(json!({
                                STRATUM: stratum,
                                ATOM_IDX: idx,
                                OP: atom_type,
                                RULE_IDX: clause_idx,
                                RULE_NAME: rule_name.to_string(),
                                OUT_BINDINGS: relation.bindings_after_eliminate().into_iter().map(|v| v.to_string()).collect_vec()
                            }));
                            idx += 1;

                            while let Some(rel) = rel_stack.pop() {
                                let (atom_type, ref_name, joins_on, filters) = match rel {
                                    r @ RelAlgebra::Fixed(..) => {
                                        if r.is_unit() {
                                            continue;
                                        }
                                        ("fixed", json!(null), json!(null), json!(null))
                                    }
                                    RelAlgebra::InMem(InMemRelationRA {
                                        storage, filters, ..
                                    }) => (
                                        "load_mem",
                                        json!(storage.rule_name.to_string()),
                                        json!(null),
                                        json!(filters.iter().map(|f| f.to_string()).collect_vec()),
                                    ),
                                    RelAlgebra::Stored(StoredRA {
                                        storage, filters, ..
                                    }) => (
                                        "load_stored",
                                        json!(format!(":{}", storage.name)),
                                        json!(null),
                                        json!(filters.iter().map(|f| f.to_string()).collect_vec()),
                                    ),
                                    RelAlgebra::Join(inner) => {
                                        if inner.left.is_unit() {
                                            rel_stack.push(&inner.right);
                                            continue;
                                        }
                                        let t = inner.join_type();
                                        let InnerJoin {
                                            left,
                                            right,
                                            joiner,
                                            ..
                                        } = inner.as_ref();
                                        rel_stack.push(left);
                                        rel_stack.push(right);
                                        (t, json!(null), json!(joiner.as_map()), json!(null))
                                    }
                                    RelAlgebra::NegJoin(inner) => {
                                        let t = inner.join_type();
                                        let NegJoin {
                                            left,
                                            right,
                                            joiner,
                                            ..
                                        } = inner.as_ref();
                                        rel_stack.push(left);
                                        rel_stack.push(right);
                                        (t, json!(null), json!(joiner.as_map()), json!(null))
                                    }
                                    RelAlgebra::Reorder(ReorderRA { relation, .. }) => {
                                        rel_stack.push(relation);
                                        ("reorder", json!(null), json!(null), json!(null))
                                    }
                                    RelAlgebra::Filter(FilteredRA { parent, pred, .. }) => {
                                        rel_stack.push(parent);
                                        (
                                            "filter",
                                            json!(null),
                                            json!(null),
                                            json!(pred.iter().map(|f| f.to_string()).collect_vec()),
                                        )
                                    }
                                    RelAlgebra::Unification(UnificationRA {
                                        parent,
                                        binding,
                                        expr,
                                        is_multi,
                                        ..
                                    }) => {
                                        rel_stack.push(parent);
                                        (
                                            if *is_multi { "multi-unify" } else { "unify" },
                                            json!(binding.name),
                                            json!(null),
                                            json!(expr.to_string()),
                                        )
                                    }
                                };
                                ret_for_relation.push(json!({
                                    STRATUM: stratum,
                                    ATOM_IDX: idx,
                                    OP: atom_type,
                                    RULE_IDX: clause_idx,
                                    RULE_NAME: rule_name.to_string(),
                                    REF_NAME: ref_name,
                                    OUT_BINDINGS: rel.bindings_after_eliminate().into_iter().map(|v| v.to_string()).collect_vec(),
                                    JOINS_ON: joins_on,
                                    FILTERS: filters,
                                }));
                                idx += 1;
                            }
                            ret_for_relation.reverse();
                            ret.extend(ret_for_relation)
                        }
                    }
                    CompiledRuleSet::Algo(_) => ret.push(json!({
                        STRATUM: stratum,
                        ATOM_IDX: 0,
                        OP: "algo",
                        RULE_IDX: 0,
                        RULE_NAME: rule_name.to_string(),
                    })),
                }
            }
        }

        let ret = ret
            .into_iter()
            .map(|m| {
                headers
                    .iter()
                    .map(|i| m.get(*i).unwrap_or(&JsonValue::Null).clone())
                    .collect_vec()
            })
            .collect_vec();

        Ok(json!({"headers": headers, "rows": ret}))
    }
    fn run_sys_op(&'s self, op: SysOp) -> Result<JsonValue> {
        match op {
            SysOp::Explain(prog) => {
                let mut tx = self.transact()?;
                let program = prog
                    .to_normalized_program(&tx)?
                    .stratify()?
                    .magic_sets_rewrite(&tx)?;
                let (compiled, _) = tx.stratified_magic_compile(&program)?;
                tx.commit_tx()?;
                self.explain_compiled(&compiled)
            }
            SysOp::Compact => {
                self.compact_relation()?;
                Ok(json!({"headers": ["status"], "rows": [["OK"]]}))
            }
            SysOp::ListRelations => self.list_relations(),
            SysOp::RemoveRelation(rel_names) => {
                let mut bounds = vec![];
                {
                    let mut tx = self.transact_write()?;
                    for rs in rel_names {
                        let bound = tx.destroy_relation(&rs)?;
                        bounds.push(bound);
                    }
                    tx.commit_tx()?;
                }
                for (lower, upper) in bounds {
                    self.db.del_range(&lower, &upper)?;
                }
                Ok(json!({"headers": ["status"], "rows": [["OK"]]}))
            }
            SysOp::ListRelation(rs) => self.list_relation(&rs),
            SysOp::RenameRelation(rename_pairs) => {
                let mut tx = self.transact_write()?;
                for (old, new) in rename_pairs {
                    tx.rename_relation(old, new)?;
                }
                tx.commit_tx()?;
                Ok(json!({"headers": ["status"], "rows": [["OK"]]}))
            }
            SysOp::ListRunning => self.list_running(),
            SysOp::KillRunning(id) => {
                let queries = self.running_queries.lock().unwrap();
                Ok(match queries.get(&id) {
                    None => {
                        json!({"headers": ["status"], "rows": [["NOT_FOUND"]]})
                    }
                    Some(handle) => {
                        handle.poison.0.store(true, Ordering::Relaxed);
                        json!({"headers": ["status"], "rows": [["KILLING"]]})
                    }
                })
            }
            SysOp::ShowTrigger(name) => {
                let mut tx = self.transact()?;
                let rel = tx.get_relation(&name, false)?;
                let mut ret = vec![];
                for (i, trigger) in rel.put_triggers.iter().enumerate() {
                    ret.push(json!(["put", i, trigger]))
                }
                for (i, trigger) in rel.rm_triggers.iter().enumerate() {
                    ret.push(json!(["rm", i, trigger]))
                }
                for (i, trigger) in rel.replace_triggers.iter().enumerate() {
                    ret.push(json!(["replace", i, trigger]))
                }
                tx.commit_tx()?;
                Ok(json!({"headers": ["type", "idx", "trigger"], "rows": ret}))
            }
            SysOp::SetTriggers(name, puts, rms, replaces) => {
                let mut tx = self.transact_write()?;
                tx.set_relation_triggers(name, puts, rms, replaces)?;
                tx.commit_tx()?;
                Ok(json!({"headers": ["status"], "rows": [["OK"]]}))
            }
            SysOp::SetAccessLevel(names, level) => {
                let mut tx = self.transact_write()?;
                for name in names {
                    tx.set_access_level(name, level)?;
                }
                tx.commit_tx()?;
                Ok(json!({"headers": ["status"], "rows": [["OK"]]}))
            }
        }
    }
    pub(crate) fn run_query(
        &self,
        tx: &mut SessionTx<'_>,
        input_program: InputProgram,
    ) -> Result<(JsonValue, Vec<(Vec<u8>, Vec<u8>)>)> {
        let mut clean_ups = vec![];
        if let Some((meta, op)) = &input_program.out_opts.store_relation {
            if *op == RelationOp::Create {
                #[derive(Debug, Error, Diagnostic)]
                #[error("Stored relation {0} conflicts with an existing one")]
                #[diagnostic(code(eval::stored_relation_conflict))]
                struct StoreRelationConflict(String);

                ensure!(
                    !tx.relation_exists(&meta.name)?,
                    StoreRelationConflict(meta.name.to_string())
                )
            } else if *op != RelationOp::Replace {
                #[derive(Debug, Error, Diagnostic)]
                #[error("Stored relation {0} not found")]
                #[diagnostic(code(eval::stored_relation_not_found))]
                struct StoreRelationNotFoundError(String);

                let existing = tx.get_relation(&meta.name, true)?;

                ensure!(
                    tx.relation_exists(&meta.name)?,
                    StoreRelationNotFoundError(meta.name.to_string())
                );

                existing.ensure_compatible(meta)?;
            }
        };
        let program = input_program
            .to_normalized_program(tx)?
            .stratify()?
            .magic_sets_rewrite(tx)?;
        let (compiled, stores) = tx.stratified_magic_compile(&program)?;

        let poison = Poison::default();
        if let Some(secs) = input_program.out_opts.timeout {
            poison.set_timeout(secs)?;
        }
        let id = self.queries_count.fetch_add(1, Ordering::AcqRel);

        #[cfg(not(feature = "wasm"))]
        let now = SystemTime::now();
        #[cfg(not(feature = "wasm"))]
        let since_the_epoch = now
            .duration_since(UNIX_EPOCH)
            .into_diagnostic()?
            .as_secs_f64();

        #[cfg(feature = "wasm")]
        let since_the_epoch = js_sys::Date::now();

        let handle = RunningQueryHandle {
            started_at: since_the_epoch,
            poison: poison.clone(),
        };
        self.running_queries.lock().unwrap().insert(id, handle);
        let _guard = RunningQueryCleanup {
            id,
            running_queries: self.running_queries.clone(),
        };

        let (result, early_return) = tx.stratified_magic_evaluate(
            &compiled,
            &stores,
            if input_program.out_opts.sorters.is_empty() {
                input_program.out_opts.num_to_take()
            } else {
                None
            },
            if input_program.out_opts.sorters.is_empty() {
                input_program.out_opts.offset
            } else {
                None
            },
            poison,
        )?;
        if let Some(assertion) = &input_program.out_opts.assertion {
            match assertion {
                QueryAssertion::AssertNone(span) => {
                    if let Some(tuple) = result.scan_all().next() {
                        let tuple = tuple?;

                        #[derive(Debug, Error, Diagnostic)]
                        #[error(
                            "The query is asserted to return no result, but a tuple {0:?} is found"
                        )]
                        #[diagnostic(code(eval::assert_none_failure))]
                        struct AssertNoneFailure(Tuple, #[label] SourceSpan);
                        bail!(AssertNoneFailure(tuple, *span))
                    }
                }
                QueryAssertion::AssertSome(span) => {
                    if let Some(tuple) = result.scan_all().next() {
                        let _ = tuple?;
                    } else {
                        #[derive(Debug, Error, Diagnostic)]
                        #[error("The query is asserted to return some results, but returned none")]
                        #[diagnostic(code(eval::assert_some_failure))]
                        struct AssertSomeFailure(#[label] SourceSpan);
                        bail!(AssertSomeFailure(*span))
                    }
                }
            }
        }
        let json_headers = match input_program.get_entry_out_head() {
            Err(_) => JsonValue::Null,
            Ok(headers) => headers.into_iter().map(|v| json!(v.name)).collect(),
        };
        if !input_program.out_opts.sorters.is_empty() {
            let entry_head = input_program.get_entry_out_head()?;
            let sorted_result =
                tx.sort_and_collect(result, &input_program.out_opts.sorters, &entry_head)?;
            let sorted_iter = if let Some(offset) = input_program.out_opts.offset {
                Left(sorted_result.into_iter().skip(offset))
            } else {
                Right(sorted_result.into_iter())
            };
            let sorted_iter = if let Some(limit) = input_program.out_opts.limit {
                Left(sorted_iter.take(limit))
            } else {
                Right(sorted_iter)
            };
            let sorted_iter = sorted_iter.map(Ok);
            if let Some((meta, relation_op)) = &input_program.out_opts.store_relation {
                let to_clear = tx
                    .execute_relation(
                        self,
                        sorted_iter,
                        *relation_op,
                        meta,
                        &input_program.get_entry_out_head_or_default()?,
                    )
                    .wrap_err_with(|| format!("when executing against relation '{}'", meta.name))?;
                clean_ups.extend(to_clear);
                Ok((json!({"headers": ["status"], "rows": [["OK"]]}), clean_ups))
            } else {
                let ret: Vec<Vec<JsonValue>> = sorted_iter
                    .map_ok(|tuple| -> Vec<JsonValue> {
                        tuple.0.into_iter().map(JsonValue::from).collect()
                    })
                    .try_collect()?;

                Ok((json!({ "rows": ret, "headers": json_headers }), clean_ups))
            }
        } else {
            let scan = if early_return {
                Right(Left(result.scan_early_returned()))
            } else if input_program.out_opts.limit.is_some()
                || input_program.out_opts.offset.is_some()
            {
                let limit = input_program.out_opts.limit.unwrap_or(usize::MAX);
                let offset = input_program.out_opts.offset.unwrap_or(0);
                Right(Right(result.scan_all().skip(offset).take(limit)))
            } else {
                Left(result.scan_all())
            };

            if let Some((meta, relation_op)) = &input_program.out_opts.store_relation {
                let to_clear = tx
                    .execute_relation(
                        self,
                        scan,
                        *relation_op,
                        meta,
                        &input_program.get_entry_out_head_or_default()?,
                    )
                    .wrap_err_with(|| format!("when executing against relation '{}'", meta.name))?;
                clean_ups.extend(to_clear);
                Ok((json!({"headers": ["status"], "rows": [["OK"]]}), clean_ups))
            } else {
                let ret: Vec<Vec<JsonValue>> = scan
                    .map_ok(|tuple| -> Vec<JsonValue> {
                        tuple.0.into_iter().map(JsonValue::from).collect()
                    })
                    .try_collect()?;

                Ok((json!({ "rows": ret, "headers": json_headers }), clean_ups))
            }
        }
    }
    pub(crate) fn list_running(&self) -> Result<JsonValue> {
        let res = self
            .running_queries
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| json!([k, format!("{:?}", v.started_at)]))
            .collect_vec();
        Ok(json!({"rows": res, "headers": ["id", "started_at"]}))
    }
    fn list_relation(&'s self, name: &str) -> Result<JsonValue> {
        let mut tx = self.transact()?;
        let handle = tx.get_relation(name, false)?;
        let mut ret = vec![];
        let mut idx = 0;
        for col in &handle.metadata.keys {
            ret.push(json!([
                col.name,
                true,
                idx,
                col.typing.to_string(),
                col.default_gen.is_some()
            ]));
            idx += 1;
        }
        for col in &handle.metadata.non_keys {
            ret.push(json!([
                col.name,
                false,
                idx,
                col.typing.to_string(),
                col.default_gen.is_some()
            ]));
            idx += 1;
        }
        tx.commit_tx()?;
        Ok(json!({"rows": ret, "headers": ["column", "is_key", "index", "type", "has_default"]}))
    }
    fn list_relations(&'s self) -> Result<JsonValue> {
        let lower =
            Tuple(vec![DataValue::Str(SmartString::from(""))]).encode_as_key(RelationId::SYSTEM);
        let upper = Tuple(vec![DataValue::Str(SmartString::from(String::from(
            LARGEST_UTF_CHAR,
        )))])
        .encode_as_key(RelationId::SYSTEM);
        let tx = self.db.transact(false)?;
        let mut collected = vec![];
        for kv_res in tx.range_scan(&lower, &upper) {
            let (k_slice, v_slice) = kv_res?;
            if upper <= k_slice {
                break;
            }
            let meta = RelationHandle::decode(&v_slice)?;
            let n_keys = meta.metadata.keys.len();
            let n_dependents = meta.metadata.non_keys.len();
            let arity = n_keys + n_dependents;
            let name = meta.name;
            let access_level = meta.access_level.to_string();
            collected.push(json!([
                name,
                arity,
                access_level,
                n_keys,
                n_dependents,
                meta.put_triggers.len(),
                meta.rm_triggers.len(),
                meta.replace_triggers.len(),
            ]));
        }
        Ok(json!({"rows": collected, "headers":
                ["name", "arity", "access_level", "n_keys", "n_non_keys", "n_put_triggers", "n_rm_triggers", "n_replace_triggers"]}))
    }
}

#[derive(Clone, Default)]
pub(crate) struct Poison(pub(crate) Arc<AtomicBool>);

impl Poison {
    #[inline(always)]
    pub(crate) fn check(&self) -> Result<()> {
        #[derive(Debug, Error, Diagnostic)]
        #[error("Process is killed before completion")]
        #[diagnostic(code(eval::killed))]
        #[diagnostic(help("A process may be killed by timeout, or explicit command"))]
        struct ProcessKilled;

        if self.0.load(Ordering::Relaxed) {
            bail!(ProcessKilled)
        }
        Ok(())
    }
    #[cfg(feature = "nothread")]
    pub(crate) fn set_timeout(&self, _secs: f64) -> Result<()> {
        bail!("Cannot set timeout when threading is disallowed");
    }
    #[cfg(not(feature = "nothread"))]
    pub(crate) fn set_timeout(&self, secs: f64) -> Result<()> {
        let pill = self.0.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_micros((secs * 1000000.) as u64));
            pill.store(true, Ordering::Relaxed);
        });
        Ok(())
    }
}
