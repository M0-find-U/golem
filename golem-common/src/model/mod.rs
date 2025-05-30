// Copyright 2024 Golem Cloud
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::{Display, Formatter};
use std::ops::Add;
use std::str::FromStr;
use std::time::Duration;

use crate::config::RetryConfig;
use crate::model::oplog::{
    IndexedResourceKey, OplogEntry, OplogIndex, TimestampedUpdateDescription, WorkerResourceId,
};
use crate::model::regions::DeletedRegions;
use crate::newtype_uuid;
use bincode::de::read::Reader;
use bincode::de::{BorrowDecoder, Decoder};
use bincode::enc::write::Writer;
use bincode::enc::Encoder;
use bincode::error::{DecodeError, EncodeError};
use bincode::{BorrowDecode, Decode, Encode};
use derive_more::FromStr;
use golem_api_grpc::proto::golem;
use golem_api_grpc::proto::golem::worker::Cursor;
use golem_wasm_ast::analysis::analysed_type::{field, record, s64, str};
use golem_wasm_ast::analysis::AnalysedType;
use golem_wasm_rpc::IntoValue;
use poem::http::Uri;
use poem_openapi::registry::{MetaSchema, MetaSchemaRef};
use poem_openapi::types::{ParseFromJSON, ParseFromParameter, ParseResult, ToJSON};
use poem_openapi::{Enum, Object, Union};
use rand::prelude::IteratorRandom;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use uuid::{uuid, Uuid};

pub mod component_metadata;
pub mod exports;
pub mod oplog;
pub mod public_oplog;
pub mod regions;
pub mod trim_date;

use crate::uri::oss::urn::WorkerUrn;
use golem_api_grpc::proto::golem::shardmanager::{
    Pod as GrpcPod, RoutingTable as GrpcRoutingTable, RoutingTableEntry as GrpcRoutingTableEntry,
};

newtype_uuid!(
    ComponentId,
    golem_api_grpc::proto::golem::component::ComponentId
);

newtype_uuid!(ProjectId, golem_api_grpc::proto::golem::common::ProjectId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Timestamp(iso8601_timestamp::Timestamp);

impl Timestamp {
    pub fn now_utc() -> Timestamp {
        Timestamp(iso8601_timestamp::Timestamp::now_utc())
    }

    pub fn to_millis(&self) -> u64 {
        self.0
            .duration_since(iso8601_timestamp::Timestamp::UNIX_EPOCH)
            .whole_milliseconds() as u64
    }
}

impl Display for Timestamp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ToJSON for Timestamp {
    fn to_json(&self) -> Option<Value> {
        Some(Value::String(self.0.to_string()))
    }
}

impl poem_openapi::types::Type for Timestamp {
    const IS_REQUIRED: bool = true;
    type RawValueType = Self;
    type RawElementValueType = Self;

    fn name() -> Cow<'static, str> {
        Cow::from("string(timestamp)")
    }

    fn schema_ref() -> MetaSchemaRef {
        MetaSchemaRef::Inline(Box::new(MetaSchema::new_with_format("string", "date-time")))
    }

    fn as_raw_value(&self) -> Option<&Self::RawValueType> {
        Some(self)
    }

    fn raw_element_iter<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = &'a Self::RawElementValueType> + 'a> {
        Box::new(self.as_raw_value().into_iter())
    }
}

impl ParseFromParameter for Timestamp {
    fn parse_from_parameter(value: &str) -> ParseResult<Self> {
        value.parse().map_err(|_| {
            poem_openapi::types::ParseError::<Timestamp>::custom(
                "Unexpected representation of timestamp".to_string(),
            )
        })
    }
}

impl ParseFromJSON for Timestamp {
    fn parse_from_json(value: Option<Value>) -> ParseResult<Self> {
        match value {
            Some(Value::String(s)) => Timestamp::parse_from_parameter(&s),
            _ => Err(poem_openapi::types::ParseError::<Timestamp>::custom(
                "Unexpected representation of timestamp".to_string(),
            )),
        }
    }
}

impl FromStr for Timestamp {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match iso8601_timestamp::Timestamp::parse(s) {
            Some(ts) => Ok(Self(ts)),
            None => Err("Invalid timestamp".to_string()),
        }
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            iso8601_timestamp::Timestamp::deserialize(deserializer).map(Self)
        } else {
            // For non-human-readable formats we assume it was an i64 representing milliseconds from epoch
            let timestamp = i64::deserialize(deserializer)?;
            Ok(Timestamp(
                iso8601_timestamp::Timestamp::UNIX_EPOCH
                    .add(Duration::from_millis(timestamp as u64)),
            ))
        }
    }
}

impl bincode::Encode for Timestamp {
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
        (self
            .0
            .duration_since(iso8601_timestamp::Timestamp::UNIX_EPOCH)
            .whole_milliseconds() as i64)
            .encode(encoder)
    }
}

impl bincode::Decode for Timestamp {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let timestamp: i64 = bincode::Decode::decode(decoder)?;
        Ok(Timestamp(
            iso8601_timestamp::Timestamp::UNIX_EPOCH.add(Duration::from_millis(timestamp as u64)),
        ))
    }
}

impl<'de> bincode::BorrowDecode<'de> for Timestamp {
    fn borrow_decode<D: BorrowDecoder<'de>>(decoder: &mut D) -> Result<Self, DecodeError> {
        let timestamp: i64 = bincode::BorrowDecode::borrow_decode(decoder)?;
        Ok(Timestamp(
            iso8601_timestamp::Timestamp::UNIX_EPOCH.add(Duration::from_millis(timestamp as u64)),
        ))
    }
}

impl From<Timestamp> for prost_types::Timestamp {
    fn from(value: Timestamp) -> Self {
        let d = value
            .0
            .duration_since(iso8601_timestamp::Timestamp::UNIX_EPOCH);
        Self {
            seconds: d.whole_seconds(),
            nanos: d.subsec_nanoseconds(),
        }
    }
}

impl From<prost_types::Timestamp> for Timestamp {
    fn from(value: prost_types::Timestamp) -> Self {
        Timestamp(
            iso8601_timestamp::Timestamp::UNIX_EPOCH
                .add(Duration::new(value.seconds as u64, value.nanos as u32)),
        )
    }
}

impl From<u64> for Timestamp {
    fn from(value: u64) -> Self {
        Timestamp(iso8601_timestamp::Timestamp::UNIX_EPOCH.add(Duration::from_millis(value)))
    }
}

pub type ComponentVersion = u64;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Encode, Decode, Serialize, Deserialize, Object)]
#[serde(rename_all = "camelCase")]
#[oai(rename_all = "camelCase")]
pub struct WorkerId {
    pub component_id: ComponentId,
    pub worker_name: String,
}

impl WorkerId {
    pub fn to_redis_key(&self) -> String {
        format!("{}:{}", self.component_id.0, self.worker_name)
    }

    pub fn uri(&self) -> String {
        WorkerUrn {
            id: self.clone().into_target_worker_id(),
        }
        .to_string()
    }

    /// The dual of `TargetWorkerId::into_worker_id`
    pub fn into_target_worker_id(self) -> TargetWorkerId {
        TargetWorkerId {
            component_id: self.component_id,
            worker_name: Some(self.worker_name),
        }
    }
}

impl FromStr for WorkerId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 2 {
            let component_id_uuid = Uuid::from_str(parts[0])
                .map_err(|_| format!("invalid component id: {s} - expected uuid"))?;
            let component_id = ComponentId(component_id_uuid);
            let worker_name = parts[1].to_string();
            Ok(Self {
                component_id,
                worker_name,
            })
        } else {
            Err(format!(
                "invalid worker id: {s} - expected format: <component_id>:<worker_name>"
            ))
        }
    }
}

impl Display for WorkerId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("{}/{}", self.component_id, self.worker_name))
    }
}

impl From<WorkerId> for golem_api_grpc::proto::golem::worker::WorkerId {
    fn from(value: WorkerId) -> Self {
        Self {
            component_id: Some(value.component_id.into()),
            name: value.worker_name,
        }
    }
}

impl TryFrom<golem_api_grpc::proto::golem::worker::WorkerId> for WorkerId {
    type Error = String;

    fn try_from(
        value: golem_api_grpc::proto::golem::worker::WorkerId,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            component_id: value.component_id.unwrap().try_into()?,
            worker_name: value.name,
        })
    }
}

impl IntoValue for WorkerId {
    fn into_value(self) -> golem_wasm_rpc::Value {
        golem_wasm_rpc::Value::Record(vec![
            self.component_id.into_value(),
            self.worker_name.into_value(),
        ])
    }

    fn get_type() -> AnalysedType {
        record(vec![
            field("component_id", ComponentId::get_type()),
            field("worker_name", std::string::String::get_type()),
        ])
    }
}

/// Associates a worker-id with its owner account
#[derive(Clone, Debug, Eq, PartialEq, Hash, Encode, Decode)]
pub struct OwnedWorkerId {
    pub account_id: AccountId,
    pub worker_id: WorkerId,
}

impl OwnedWorkerId {
    pub fn new(account_id: &AccountId, worker_id: &WorkerId) -> Self {
        Self {
            account_id: account_id.clone(),
            worker_id: worker_id.clone(),
        }
    }

    pub fn worker_id(&self) -> WorkerId {
        self.worker_id.clone()
    }

    pub fn account_id(&self) -> AccountId {
        self.account_id.clone()
    }

    pub fn component_id(&self) -> ComponentId {
        self.worker_id.component_id.clone()
    }

    pub fn worker_name(&self) -> String {
        self.worker_id.worker_name.clone()
    }
}

impl Display for OwnedWorkerId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.account_id, self.worker_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, Encode, Decode)]
pub struct TargetWorkerId {
    pub component_id: ComponentId,
    pub worker_name: Option<String>,
}

impl TargetWorkerId {
    pub fn uri(&self) -> String {
        WorkerUrn { id: self.clone() }.to_string()
    }

    /// Converts a `TargetWorkerId` to a `WorkerId` if the worker name is specified
    pub fn try_into_worker_id(self) -> Option<WorkerId> {
        self.worker_name.map(|worker_name| WorkerId {
            component_id: self.component_id,
            worker_name,
        })
    }

    /// Converts a `TargetWorkerId` to a `WorkerId`. If the worker name was not specified,
    /// it generates a new unique one, and if the `force_in_shard` set is not empty, it guarantees
    /// that the generated worker ID will belong to one of the provided shards.
    ///
    /// If the worker name was specified, `force_in_shard` is ignored.
    pub fn into_worker_id(
        self,
        force_in_shard: &HashSet<ShardId>,
        number_of_shards: usize,
    ) -> WorkerId {
        let TargetWorkerId {
            component_id,
            worker_name,
        } = self;
        match worker_name {
            Some(worker_name) => WorkerId {
                component_id,
                worker_name,
            },
            None => {
                if force_in_shard.is_empty() || number_of_shards == 0 {
                    let worker_name = Uuid::new_v4().to_string();
                    WorkerId {
                        component_id,
                        worker_name,
                    }
                } else {
                    let mut current = Uuid::new_v4().to_u128_le();
                    loop {
                        let uuid = Uuid::from_u128_le(current);
                        let worker_name = uuid.to_string();
                        let worker_id = WorkerId {
                            component_id: component_id.clone(),
                            worker_name,
                        };
                        let shard_id = ShardId::from_worker_id(&worker_id, number_of_shards);
                        if force_in_shard.contains(&shard_id) {
                            return worker_id;
                        }
                        current += 1;
                    }
                }
            }
        }
    }
}

impl Display for TargetWorkerId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.worker_name {
            Some(worker_name) => write!(f, "{}/{}", self.component_id, worker_name),
            None => write!(f, "{}/*", self.component_id),
        }
    }
}

impl From<WorkerId> for TargetWorkerId {
    fn from(value: WorkerId) -> Self {
        value.into_target_worker_id()
    }
}

impl From<&WorkerId> for TargetWorkerId {
    fn from(value: &WorkerId) -> Self {
        value.clone().into_target_worker_id()
    }
}

impl TryFrom<golem::worker::TargetWorkerId> for TargetWorkerId {
    type Error = String;

    fn try_from(value: golem::worker::TargetWorkerId) -> Result<Self, Self::Error> {
        Ok(Self {
            component_id: value
                .component_id
                .ok_or("Missing component_id")?
                .try_into()?,
            worker_name: value.name,
        })
    }
}

impl From<TargetWorkerId> for golem::worker::TargetWorkerId {
    fn from(value: TargetWorkerId) -> Self {
        Self {
            component_id: Some(value.component_id.into()),
            name: value.worker_name,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Encode, Decode, Serialize, Deserialize, Object)]
#[serde(rename_all = "camelCase")]
#[oai(rename_all = "camelCase")]
pub struct PromiseId {
    pub worker_id: WorkerId,
    pub oplog_idx: OplogIndex,
}

impl PromiseId {
    pub fn to_redis_key(&self) -> String {
        format!("{}:{}", self.worker_id.to_redis_key(), self.oplog_idx)
    }
}

impl From<PromiseId> for golem_api_grpc::proto::golem::worker::PromiseId {
    fn from(value: PromiseId) -> Self {
        Self {
            worker_id: Some(value.worker_id.into()),
            oplog_idx: value.oplog_idx.into(),
        }
    }
}

impl TryFrom<golem_api_grpc::proto::golem::worker::PromiseId> for PromiseId {
    type Error = String;

    fn try_from(
        value: golem_api_grpc::proto::golem::worker::PromiseId,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            worker_id: value.worker_id.ok_or("Missing worker_id")?.try_into()?,
            oplog_idx: OplogIndex::from_u64(value.oplog_idx),
        })
    }
}

impl Display for PromiseId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.worker_id, self.oplog_idx)
    }
}

impl IntoValue for PromiseId {
    fn into_value(self) -> golem_wasm_rpc::Value {
        golem_wasm_rpc::Value::Record(vec![
            self.worker_id.into_value(),
            self.oplog_idx.into_value(),
        ])
    }

    fn get_type() -> AnalysedType {
        record(vec![
            field("worker_id", WorkerId::get_type()),
            field("oplog_idx", OplogIndex::get_type()),
        ])
    }
}

/// Actions that can be scheduled to be executed at a given point in time
#[derive(Debug, Clone, Hash, Eq, PartialEq, Encode, Decode)]
pub enum ScheduledAction {
    /// Completes a given promise
    CompletePromise {
        account_id: AccountId,
        promise_id: PromiseId,
    },
    /// Archives all entries from the first non-empty layer of an oplog to the next layer,
    /// if the last oplog index did not change. If there are more layers below, schedules
    /// a next action to archive the next layer.
    ArchiveOplog {
        owned_worker_id: OwnedWorkerId,
        last_oplog_index: OplogIndex,
        next_after: Duration,
    },
}

impl ScheduledAction {
    pub fn owned_worker_id(&self) -> OwnedWorkerId {
        match self {
            ScheduledAction::CompletePromise {
                account_id,
                promise_id,
            } => OwnedWorkerId::new(account_id, &promise_id.worker_id),
            ScheduledAction::ArchiveOplog {
                owned_worker_id, ..
            } => owned_worker_id.clone(),
        }
    }
}

impl Display for ScheduledAction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduledAction::CompletePromise { promise_id, .. } => {
                write!(f, "complete[{}]", promise_id)
            }
            ScheduledAction::ArchiveOplog {
                owned_worker_id, ..
            } => {
                write!(f, "archive[{}]", owned_worker_id)
            }
        }
    }
}

#[derive(Debug, Encode, Decode)]
pub struct ScheduleId {
    pub timestamp: i64,
    pub action: ScheduledAction,
}

impl Display for ScheduleId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.action, self.timestamp)
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
    Object,
)]
pub struct ShardId {
    value: i64,
}

impl ShardId {
    pub fn new(value: i64) -> Self {
        Self { value }
    }

    pub fn from_worker_id(worker_id: &WorkerId, number_of_shards: usize) -> Self {
        let hash = Self::hash_worker_id(worker_id);
        let value = hash.abs() % number_of_shards as i64;
        Self { value }
    }

    pub fn hash_worker_id(worker_id: &WorkerId) -> i64 {
        let (high_bits, low_bits) = (
            (worker_id.component_id.0.as_u128() >> 64) as i64,
            worker_id.component_id.0.as_u128() as i64,
        );
        let high = Self::hash_string(&high_bits.to_string());
        let worker_name = &worker_id.worker_name;
        let component_worker_name = format!("{}{}", low_bits, worker_name);
        let low = Self::hash_string(&component_worker_name);
        ((high as i64) << 32) | ((low as i64) & 0xFFFFFFFF)
    }

    fn hash_string(string: &str) -> i32 {
        let mut hash = 0;
        if hash == 0 && !string.is_empty() {
            for val in &mut string.bytes() {
                hash = 31_i32.wrapping_mul(hash).wrapping_add(val as i32);
            }
        }
        hash
    }

    pub fn is_left_neighbor(&self, other: &ShardId) -> bool {
        other.value == self.value + 1
    }
}

impl Display for ShardId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{}>", self.value)
    }
}

impl From<ShardId> for golem_api_grpc::proto::golem::shardmanager::ShardId {
    fn from(value: ShardId) -> golem_api_grpc::proto::golem::shardmanager::ShardId {
        golem_api_grpc::proto::golem::shardmanager::ShardId { value: value.value }
    }
}

impl From<golem_api_grpc::proto::golem::shardmanager::ShardId> for ShardId {
    fn from(proto: golem_api_grpc::proto::golem::shardmanager::ShardId) -> Self {
        Self { value: proto.value }
    }
}

impl IntoValue for ShardId {
    fn into_value(self) -> golem_wasm_rpc::Value {
        golem_wasm_rpc::Value::S64(self.value)
    }

    fn get_type() -> AnalysedType {
        s64()
    }
}

#[derive(Clone)]
pub struct NumberOfShards {
    pub value: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pod {
    host: String,
    port: u16,
}

impl Pod {
    pub fn uri(&self) -> Uri {
        Uri::builder()
            .scheme("http")
            .authority(format!("{}:{}", self.host, self.port).as_str())
            .path_and_query("/")
            .build()
            .expect("Failed to build URI")
    }

    pub fn uri_02(&self) -> http_02::Uri {
        http_02::Uri::builder()
            .scheme("http")
            .authority(format!("{}:{}", self.host, self.port).as_str())
            .path_and_query("/")
            .build()
            .expect("Failed to build URI")
    }
}

impl From<GrpcPod> for Pod {
    fn from(value: GrpcPod) -> Self {
        Self {
            host: value.host,
            port: value.port as u16,
        }
    }
}

#[derive(Clone)]
pub struct RoutingTable {
    pub number_of_shards: NumberOfShards,
    shard_assignments: HashMap<ShardId, Pod>,
}

impl RoutingTable {
    pub fn lookup(&self, worker_id: &WorkerId) -> Option<&Pod> {
        self.shard_assignments.get(&ShardId::from_worker_id(
            &worker_id.clone(),
            self.number_of_shards.value,
        ))
    }

    pub fn random(&self) -> Option<&Pod> {
        self.shard_assignments
            .values()
            .choose(&mut rand::thread_rng())
    }

    pub fn first(&self) -> Option<&Pod> {
        self.shard_assignments.values().next()
    }

    pub fn all(&self) -> HashSet<&Pod> {
        self.shard_assignments.values().collect()
    }
}

impl From<GrpcRoutingTable> for RoutingTable {
    fn from(value: GrpcRoutingTable) -> Self {
        Self {
            number_of_shards: NumberOfShards {
                value: value.number_of_shards as usize,
            },
            shard_assignments: value
                .shard_assignments
                .into_iter()
                .map(RoutingTableEntry::from)
                .map(|routing_table_entry| (routing_table_entry.shard_id, routing_table_entry.pod))
                .collect(),
        }
    }
}

pub struct RoutingTableEntry {
    shard_id: ShardId,
    pod: Pod,
}

impl From<GrpcRoutingTableEntry> for RoutingTableEntry {
    fn from(value: GrpcRoutingTableEntry) -> Self {
        Self {
            shard_id: value.shard_id.unwrap().into(),
            pod: value.pod.unwrap().into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ShardAssignment {
    pub number_of_shards: usize,
    pub shard_ids: HashSet<ShardId>,
}

impl ShardAssignment {
    pub fn new(number_of_shards: usize, shard_ids: HashSet<ShardId>) -> Self {
        Self {
            number_of_shards,
            shard_ids,
        }
    }

    pub fn assign_shards(&mut self, shard_ids: &HashSet<ShardId>) {
        for shard_id in shard_ids {
            self.shard_ids.insert(*shard_id);
        }
    }

    pub fn register(&mut self, number_of_shards: usize, shard_ids: &HashSet<ShardId>) {
        self.number_of_shards = number_of_shards;
        for shard_id in shard_ids {
            self.shard_ids.insert(*shard_id);
        }
    }

    pub fn revoke_shards(&mut self, shard_ids: &HashSet<ShardId>) {
        for shard_id in shard_ids {
            self.shard_ids.remove(shard_id);
        }
    }
}

impl Display for ShardAssignment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let shard_ids = self
            .shard_ids
            .iter()
            .map(|shard_id| shard_id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        write!(
            f,
            "{{ number_of_shards: {}, shard_ids: {} }}",
            self.number_of_shards, shard_ids
        )
    }
}

#[derive(Clone, Debug, Encode, Decode, Eq, Hash, PartialEq)]
pub struct IdempotencyKey {
    pub value: String,
}

impl IdempotencyKey {
    const ROOT_NS: Uuid = uuid!("9C19B15A-C83D-46F7-9BC3-EAD7923733F4");

    pub fn new(value: String) -> Self {
        Self { value }
    }

    pub fn from_uuid(value: Uuid) -> Self {
        Self {
            value: value.to_string(),
        }
    }

    pub fn fresh() -> Self {
        Self::from_uuid(Uuid::new_v4())
    }

    /// Generates a deterministic new idempotency key using a base idempotency key and an oplog index.
    ///
    /// The base idempotency key determines the "namespace" of the generated key UUIDv5. If
    /// the base idempotency key is already an UUID, it is directly used as the namespace of the v5 algorithm,
    /// while the name part is derived from the given oplog index.
    ///
    /// If the base idempotency key is not an UUID (as it can be an arbitrary user-provided string), then first
    /// we generate a UUIDv5 in the ROOT_NS namespace and use that as unique namespace for generating
    /// the new idempotency key.
    pub fn derived(base: &IdempotencyKey, oplog_index: OplogIndex) -> Self {
        let namespace = if let Ok(base_uuid) = Uuid::parse_str(&base.value) {
            base_uuid
        } else {
            Uuid::new_v5(&Self::ROOT_NS, base.value.as_bytes())
        };
        let name = format!("oplog-index-{}", oplog_index);
        Self::from_uuid(Uuid::new_v5(&namespace, name.as_bytes()))
    }
}

impl Serialize for IdempotencyKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D>(deserializer: D) -> Result<IdempotencyKey, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(IdempotencyKey { value })
    }
}

impl From<golem_api_grpc::proto::golem::worker::IdempotencyKey> for IdempotencyKey {
    fn from(proto: golem_api_grpc::proto::golem::worker::IdempotencyKey) -> Self {
        Self { value: proto.value }
    }
}

impl From<IdempotencyKey> for golem_api_grpc::proto::golem::worker::IdempotencyKey {
    fn from(value: IdempotencyKey) -> Self {
        Self { value: value.value }
    }
}

impl IntoValue for IdempotencyKey {
    fn into_value(self) -> golem_wasm_rpc::Value {
        golem_wasm_rpc::Value::String(self.value)
    }

    fn get_type() -> AnalysedType {
        str()
    }
}

impl poem_openapi::types::Type for IdempotencyKey {
    const IS_REQUIRED: bool = true;
    type RawValueType = Self;
    type RawElementValueType = Self;

    fn name() -> Cow<'static, str> {
        Cow::from(format!("string({})", stringify!(InvocationKey)))
    }

    fn schema_ref() -> MetaSchemaRef {
        MetaSchemaRef::Inline(Box::new(MetaSchema::new("string")))
    }

    fn as_raw_value(&self) -> Option<&Self::RawValueType> {
        Some(self)
    }

    fn raw_element_iter<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = &'a Self::RawElementValueType> + 'a> {
        Box::new(self.as_raw_value().into_iter())
    }
}

impl ParseFromParameter for IdempotencyKey {
    fn parse_from_parameter(value: &str) -> ParseResult<Self> {
        Ok(Self {
            value: value.to_string(),
        })
    }
}

impl ParseFromJSON for IdempotencyKey {
    fn parse_from_json(value: Option<Value>) -> ParseResult<Self> {
        match value {
            Some(Value::String(s)) => Ok(Self { value: s }),
            _ => Err(poem_openapi::types::ParseError::<IdempotencyKey>::custom(
                format!("Unexpected representation of {}", stringify!(InvocationKey)),
            )),
        }
    }
}

impl ToJSON for IdempotencyKey {
    fn to_json(&self) -> Option<Value> {
        Some(Value::String(self.value.clone()))
    }
}

impl Display for IdempotencyKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.value)
    }
}

#[derive(Clone, Debug)]
pub struct WorkerMetadata {
    pub worker_id: WorkerId,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub account_id: AccountId,
    pub created_at: Timestamp,
    pub parent: Option<WorkerId>,
    pub last_known_status: WorkerStatusRecord,
}

impl WorkerMetadata {
    pub fn default(worker_id: WorkerId, account_id: AccountId) -> WorkerMetadata {
        WorkerMetadata {
            worker_id,
            args: vec![],
            env: vec![],
            account_id,
            created_at: Timestamp::now_utc(),
            parent: None,
            last_known_status: WorkerStatusRecord::default(),
        }
    }

    pub fn owned_worker_id(&self) -> OwnedWorkerId {
        OwnedWorkerId::new(&self.account_id, &self.worker_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WorkerResourceDescription {
    pub created_at: Timestamp,
    pub indexed_resource_key: Option<IndexedResourceKey>,
}

/// Contains status information about a worker according to a given oplog index.
/// This status is just cached information, all fields must be computable by the oplog alone.
/// By having an associated oplog_idx, the cached information can be used together with the
/// tail of the oplog to determine the actual status of the worker.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct WorkerStatusRecord {
    pub status: WorkerStatus,
    pub deleted_regions: DeletedRegions,
    pub overridden_retry_config: Option<RetryConfig>,
    pub pending_invocations: Vec<TimestampedWorkerInvocation>,
    pub pending_updates: VecDeque<TimestampedUpdateDescription>,
    pub failed_updates: Vec<FailedUpdateRecord>,
    pub successful_updates: Vec<SuccessfulUpdateRecord>,
    pub invocation_results: HashMap<IdempotencyKey, OplogIndex>,
    pub current_idempotency_key: Option<IdempotencyKey>,
    pub component_version: ComponentVersion,
    pub component_size: u64,
    pub total_linear_memory_size: u64,
    pub owned_resources: HashMap<WorkerResourceId, WorkerResourceDescription>,
    pub oplog_idx: OplogIndex,
}

impl Default for WorkerStatusRecord {
    fn default() -> Self {
        WorkerStatusRecord {
            status: WorkerStatus::Idle,
            deleted_regions: DeletedRegions::new(),
            overridden_retry_config: None,
            pending_invocations: Vec::new(),
            pending_updates: VecDeque::new(),
            failed_updates: Vec::new(),
            successful_updates: Vec::new(),
            invocation_results: HashMap::new(),
            current_idempotency_key: None,
            component_version: 0,
            component_size: 0,
            total_linear_memory_size: 0,
            owned_resources: HashMap::new(),
            oplog_idx: OplogIndex::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct FailedUpdateRecord {
    pub timestamp: Timestamp,
    pub target_version: ComponentVersion,
    pub details: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SuccessfulUpdateRecord {
    pub timestamp: Timestamp,
    pub target_version: ComponentVersion,
}

/// Represents last known status of a worker
///
/// This is always recorded together with the current oplog index, and it can only be used
/// as a source of truth if there are no newer oplog entries since the record.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Enum)]
pub enum WorkerStatus {
    /// The worker is running an invoked function
    Running,
    /// The worker is ready to run an invoked function
    Idle,
    /// An invocation is active but waiting for something (sleeping, waiting for a promise)
    Suspended,
    /// The last invocation was interrupted but will be resumed
    Interrupted,
    /// The last invocation failed and a retry was scheduled
    Retrying,
    /// The last invocation failed and the worker can no longer be used
    Failed,
    /// The worker exited after a successful invocation and can no longer be invoked
    Exited,
}

impl PartialOrd for WorkerStatus {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WorkerStatus {
    fn cmp(&self, other: &Self) -> Ordering {
        let v1: i32 = self.clone().into();
        let v2: i32 = other.clone().into();
        v1.cmp(&v2)
    }
}

impl FromStr for WorkerStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "running" => Ok(WorkerStatus::Running),
            "idle" => Ok(WorkerStatus::Idle),
            "suspended" => Ok(WorkerStatus::Suspended),
            "interrupted" => Ok(WorkerStatus::Interrupted),
            "retrying" => Ok(WorkerStatus::Retrying),
            "failed" => Ok(WorkerStatus::Failed),
            "exited" => Ok(WorkerStatus::Exited),
            _ => Err(format!("Unknown worker status: {}", s)),
        }
    }
}

impl Display for WorkerStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerStatus::Running => write!(f, "Running"),
            WorkerStatus::Idle => write!(f, "Idle"),
            WorkerStatus::Suspended => write!(f, "Suspended"),
            WorkerStatus::Interrupted => write!(f, "Interrupted"),
            WorkerStatus::Retrying => write!(f, "Retrying"),
            WorkerStatus::Failed => write!(f, "Failed"),
            WorkerStatus::Exited => write!(f, "Exited"),
        }
    }
}

impl From<WorkerStatus> for golem_api_grpc::proto::golem::worker::WorkerStatus {
    fn from(value: WorkerStatus) -> Self {
        match value {
            WorkerStatus::Running => golem_api_grpc::proto::golem::worker::WorkerStatus::Running,
            WorkerStatus::Idle => golem_api_grpc::proto::golem::worker::WorkerStatus::Idle,
            WorkerStatus::Suspended => {
                golem_api_grpc::proto::golem::worker::WorkerStatus::Suspended
            }
            WorkerStatus::Interrupted => {
                golem_api_grpc::proto::golem::worker::WorkerStatus::Interrupted
            }
            WorkerStatus::Retrying => golem_api_grpc::proto::golem::worker::WorkerStatus::Retrying,
            WorkerStatus::Failed => golem_api_grpc::proto::golem::worker::WorkerStatus::Failed,
            WorkerStatus::Exited => golem_api_grpc::proto::golem::worker::WorkerStatus::Exited,
        }
    }
}

impl TryFrom<i32> for WorkerStatus {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(WorkerStatus::Running),
            1 => Ok(WorkerStatus::Idle),
            2 => Ok(WorkerStatus::Suspended),
            3 => Ok(WorkerStatus::Interrupted),
            4 => Ok(WorkerStatus::Retrying),
            5 => Ok(WorkerStatus::Failed),
            6 => Ok(WorkerStatus::Exited),
            _ => Err(format!("Unknown worker status: {}", value)),
        }
    }
}

impl From<WorkerStatus> for i32 {
    fn from(value: WorkerStatus) -> Self {
        match value {
            WorkerStatus::Running => 0,
            WorkerStatus::Idle => 1,
            WorkerStatus::Suspended => 2,
            WorkerStatus::Interrupted => 3,
            WorkerStatus::Retrying => 4,
            WorkerStatus::Failed => 5,
            WorkerStatus::Exited => 6,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum WorkerInvocation {
    ExportedFunction {
        idempotency_key: IdempotencyKey,
        full_function_name: String,
        function_input: Vec<golem_wasm_rpc::Value>,
    },
    ManualUpdate {
        target_version: ComponentVersion,
    },
}

impl WorkerInvocation {
    pub fn is_idempotency_key(&self, key: &IdempotencyKey) -> bool {
        match self {
            Self::ExportedFunction {
                idempotency_key, ..
            } => idempotency_key == key,
            _ => false,
        }
    }

    pub fn idempotency_key(&self) -> Option<&IdempotencyKey> {
        match self {
            Self::ExportedFunction {
                idempotency_key, ..
            } => Some(idempotency_key),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct TimestampedWorkerInvocation {
    pub timestamp: Timestamp,
    pub invocation: WorkerInvocation,
}

#[derive(
    Clone,
    Debug,
    PartialOrd,
    Ord,
    FromStr,
    Eq,
    Hash,
    PartialEq,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(transparent)]
pub struct AccountId {
    pub value: String,
}

impl AccountId {
    pub fn generate() -> Self {
        Self {
            value: Uuid::new_v4().to_string(),
        }
    }
}

impl From<&str> for AccountId {
    fn from(value: &str) -> Self {
        Self {
            value: value.to_string(),
        }
    }
}

impl From<golem_api_grpc::proto::golem::common::AccountId> for AccountId {
    fn from(proto: golem_api_grpc::proto::golem::common::AccountId) -> Self {
        Self { value: proto.name }
    }
}

impl From<AccountId> for golem_api_grpc::proto::golem::common::AccountId {
    fn from(value: AccountId) -> Self {
        golem_api_grpc::proto::golem::common::AccountId { name: value.value }
    }
}

impl poem_openapi::types::Type for AccountId {
    const IS_REQUIRED: bool = true;
    type RawValueType = Self;
    type RawElementValueType = Self;

    fn name() -> Cow<'static, str> {
        Cow::from("string(account_id)")
    }

    fn schema_ref() -> MetaSchemaRef {
        MetaSchemaRef::Inline(Box::new(MetaSchema::new("string")))
    }

    fn as_raw_value(&self) -> Option<&Self::RawValueType> {
        Some(self)
    }

    fn raw_element_iter<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = &'a Self::RawElementValueType> + 'a> {
        Box::new(self.as_raw_value().into_iter())
    }
}

impl ParseFromParameter for AccountId {
    fn parse_from_parameter(value: &str) -> ParseResult<Self> {
        Ok(Self {
            value: value.to_string(),
        })
    }
}

impl ParseFromJSON for AccountId {
    fn parse_from_json(value: Option<Value>) -> ParseResult<Self> {
        match value {
            Some(Value::String(s)) => Ok(Self { value: s }),
            _ => Err(poem_openapi::types::ParseError::<AccountId>::custom(
                "Unexpected representation of AccountId".to_string(),
            )),
        }
    }
}

impl ToJSON for AccountId {
    fn to_json(&self) -> Option<Value> {
        Some(Value::String(self.value.clone()))
    }
}

impl Display for AccountId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Object)]
pub struct WorkerNameFilter {
    pub comparator: StringFilterComparator,
    pub value: String,
}

impl WorkerNameFilter {
    pub fn new(comparator: StringFilterComparator, value: String) -> Self {
        Self { comparator, value }
    }
}

impl Display for WorkerNameFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "name {} {}", self.comparator, self.value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Object)]
pub struct WorkerStatusFilter {
    pub comparator: FilterComparator,
    pub value: WorkerStatus,
}

impl WorkerStatusFilter {
    pub fn new(comparator: FilterComparator, value: WorkerStatus) -> Self {
        Self { comparator, value }
    }
}

impl Display for WorkerStatusFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "status == {:?}", self.value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Object)]
pub struct WorkerVersionFilter {
    pub comparator: FilterComparator,
    pub value: ComponentVersion,
}

impl WorkerVersionFilter {
    pub fn new(comparator: FilterComparator, value: ComponentVersion) -> Self {
        Self { comparator, value }
    }
}

impl Display for WorkerVersionFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "version {} {}", self.comparator, self.value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Object)]
pub struct WorkerCreatedAtFilter {
    pub comparator: FilterComparator,
    pub value: Timestamp,
}

impl WorkerCreatedAtFilter {
    pub fn new(comparator: FilterComparator, value: Timestamp) -> Self {
        Self { comparator, value }
    }
}

impl Display for WorkerCreatedAtFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "created_at {} {}", self.comparator, self.value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Object)]
pub struct WorkerEnvFilter {
    pub name: String,
    pub comparator: StringFilterComparator,
    pub value: String,
}

impl WorkerEnvFilter {
    pub fn new(name: String, comparator: StringFilterComparator, value: String) -> Self {
        Self {
            name,
            comparator,
            value,
        }
    }
}

impl Display for WorkerEnvFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "env.{} {} {}", self.name, self.comparator, self.value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Object)]
pub struct WorkerAndFilter {
    pub filters: Vec<WorkerFilter>,
}

impl WorkerAndFilter {
    pub fn new(filters: Vec<WorkerFilter>) -> Self {
        Self { filters }
    }
}

impl Display for WorkerAndFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "({})",
            self.filters
                .iter()
                .map(|f| f.clone().to_string())
                .collect::<Vec<String>>()
                .join(" AND ")
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Object)]
pub struct WorkerOrFilter {
    pub filters: Vec<WorkerFilter>,
}

impl WorkerOrFilter {
    pub fn new(filters: Vec<WorkerFilter>) -> Self {
        Self { filters }
    }
}

impl Display for WorkerOrFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "({})",
            self.filters
                .iter()
                .map(|f| f.clone().to_string())
                .collect::<Vec<String>>()
                .join(" OR ")
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Object)]
pub struct WorkerNotFilter {
    filter: Box<WorkerFilter>,
}

impl WorkerNotFilter {
    pub fn new(filter: WorkerFilter) -> Self {
        Self {
            filter: Box::new(filter),
        }
    }
}

impl Display for WorkerNotFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "NOT ({})", self.filter)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Union)]
#[oai(discriminator_name = "type", one_of = true)]
#[serde(tag = "type")]
pub enum WorkerFilter {
    Name(WorkerNameFilter),
    Status(WorkerStatusFilter),
    Version(WorkerVersionFilter),
    CreatedAt(WorkerCreatedAtFilter),
    Env(WorkerEnvFilter),
    And(WorkerAndFilter),
    Or(WorkerOrFilter),
    Not(WorkerNotFilter),
}

impl WorkerFilter {
    pub fn and(&self, filter: WorkerFilter) -> Self {
        match self.clone() {
            WorkerFilter::And(WorkerAndFilter { filters }) => {
                Self::new_and([filters, vec![filter]].concat())
            }
            f => Self::new_and(vec![f, filter]),
        }
    }

    pub fn or(&self, filter: WorkerFilter) -> Self {
        match self.clone() {
            WorkerFilter::Or(WorkerOrFilter { filters }) => {
                Self::new_or([filters, vec![filter]].concat())
            }
            f => Self::new_or(vec![f, filter]),
        }
    }

    pub fn not(&self) -> Self {
        Self::new_not(self.clone())
    }

    pub fn matches(&self, metadata: &WorkerMetadata) -> bool {
        match self.clone() {
            WorkerFilter::Name(WorkerNameFilter { comparator, value }) => {
                comparator.matches(&metadata.worker_id.worker_name, &value)
            }
            WorkerFilter::Version(WorkerVersionFilter { comparator, value }) => {
                let version: ComponentVersion = metadata.last_known_status.component_version;
                comparator.matches(&version, &value)
            }
            WorkerFilter::Env(WorkerEnvFilter {
                name,
                comparator,
                value,
            }) => {
                let mut result = false;
                let name = name.to_lowercase();
                for env_value in metadata.env.clone() {
                    if env_value.0.to_lowercase() == name {
                        result = comparator.matches(&env_value.1, &value);

                        break;
                    }
                }
                result
            }
            WorkerFilter::CreatedAt(WorkerCreatedAtFilter { comparator, value }) => {
                comparator.matches(&metadata.created_at, &value)
            }
            WorkerFilter::Status(WorkerStatusFilter { comparator, value }) => {
                comparator.matches(&metadata.last_known_status.status, &value)
            }
            WorkerFilter::Not(WorkerNotFilter { filter }) => !filter.matches(metadata),
            WorkerFilter::And(WorkerAndFilter { filters }) => {
                let mut result = true;
                for filter in filters {
                    if !filter.matches(metadata) {
                        result = false;
                        break;
                    }
                }
                result
            }
            WorkerFilter::Or(WorkerOrFilter { filters }) => {
                let mut result = true;
                if !filters.is_empty() {
                    result = false;
                    for filter in filters {
                        if filter.matches(metadata) {
                            result = true;
                            break;
                        }
                    }
                }
                result
            }
        }
    }

    pub fn new_and(filters: Vec<WorkerFilter>) -> Self {
        WorkerFilter::And(WorkerAndFilter::new(filters))
    }

    pub fn new_or(filters: Vec<WorkerFilter>) -> Self {
        WorkerFilter::Or(WorkerOrFilter::new(filters))
    }

    pub fn new_not(filter: WorkerFilter) -> Self {
        WorkerFilter::Not(WorkerNotFilter::new(filter))
    }

    pub fn new_name(comparator: StringFilterComparator, value: String) -> Self {
        WorkerFilter::Name(WorkerNameFilter::new(comparator, value))
    }

    pub fn new_env(name: String, comparator: StringFilterComparator, value: String) -> Self {
        WorkerFilter::Env(WorkerEnvFilter::new(name, comparator, value))
    }

    pub fn new_version(comparator: FilterComparator, value: ComponentVersion) -> Self {
        WorkerFilter::Version(WorkerVersionFilter::new(comparator, value))
    }

    pub fn new_status(comparator: FilterComparator, value: WorkerStatus) -> Self {
        WorkerFilter::Status(WorkerStatusFilter::new(comparator, value))
    }

    pub fn new_created_at(comparator: FilterComparator, value: Timestamp) -> Self {
        WorkerFilter::CreatedAt(WorkerCreatedAtFilter::new(comparator, value))
    }

    pub fn from(filters: Vec<String>) -> Result<WorkerFilter, String> {
        let mut fs = Vec::new();
        for f in filters {
            fs.push(WorkerFilter::from_str(&f)?);
        }
        Ok(WorkerFilter::new_and(fs))
    }
}

impl Display for WorkerFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerFilter::Name(filter) => {
                write!(f, "{}", filter)
            }
            WorkerFilter::Version(filter) => {
                write!(f, "{}", filter)
            }
            WorkerFilter::Status(filter) => {
                write!(f, "{}", filter)
            }
            WorkerFilter::CreatedAt(filter) => {
                write!(f, "{}", filter)
            }
            WorkerFilter::Env(filter) => {
                write!(f, "{}", filter)
            }
            WorkerFilter::Not(filter) => {
                write!(f, "{}", filter)
            }
            WorkerFilter::And(filter) => {
                write!(f, "{}", filter)
            }
            WorkerFilter::Or(filter) => {
                write!(f, "{}", filter)
            }
        }
    }
}

impl FromStr for WorkerFilter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let elements = s.split_whitespace().collect::<Vec<&str>>();

        if elements.len() == 3 {
            let arg = elements[0];
            let comparator = elements[1];
            let value = elements[2];
            match arg {
                "name" => Ok(WorkerFilter::new_name(
                    comparator.parse()?,
                    value.to_string(),
                )),
                "version" => Ok(WorkerFilter::new_version(
                    comparator.parse()?,
                    value
                        .parse()
                        .map_err(|e| format!("Invalid filter value: {}", e))?,
                )),
                "status" => Ok(WorkerFilter::new_status(
                    comparator.parse()?,
                    value.parse()?,
                )),
                "created_at" | "createdAt" => Ok(WorkerFilter::new_created_at(
                    comparator.parse()?,
                    value.parse()?,
                )),
                _ if arg.starts_with("env.") => {
                    let name = &arg[4..];
                    Ok(WorkerFilter::new_env(
                        name.to_string(),
                        comparator.parse()?,
                        value.to_string(),
                    ))
                }
                _ => Err(format!("Invalid filter: {}", s)),
            }
        } else {
            Err(format!("Invalid filter: {}", s))
        }
    }
}

impl TryFrom<golem_api_grpc::proto::golem::worker::WorkerFilter> for WorkerFilter {
    type Error = String;

    fn try_from(
        value: golem_api_grpc::proto::golem::worker::WorkerFilter,
    ) -> Result<Self, Self::Error> {
        match value.filter {
            Some(filter) => match filter {
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Name(filter) => Ok(
                    WorkerFilter::new_name(filter.comparator.try_into()?, filter.value),
                ),
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Version(filter) => Ok(
                    WorkerFilter::new_version(filter.comparator.try_into()?, filter.value),
                ),
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Status(filter) => {
                    Ok(WorkerFilter::new_status(
                        filter.comparator.try_into()?,
                        filter.value.try_into()?,
                    ))
                }
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::CreatedAt(filter) => {
                    let value = filter
                        .value
                        .map(|t| t.into())
                        .ok_or_else(|| "Missing value".to_string())?;
                    Ok(WorkerFilter::new_created_at(
                        filter.comparator.try_into()?,
                        value,
                    ))
                }
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Env(filter) => Ok(
                    WorkerFilter::new_env(filter.name, filter.comparator.try_into()?, filter.value),
                ),
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Not(filter) => {
                    let filter = *filter.filter.ok_or_else(|| "Missing filter".to_string())?;
                    Ok(WorkerFilter::new_not(filter.try_into()?))
                }
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::And(
                    golem_api_grpc::proto::golem::worker::WorkerAndFilter { filters },
                ) => {
                    let filters = filters.into_iter().map(|f| f.try_into()).collect::<Result<
                        Vec<WorkerFilter>,
                        String,
                    >>(
                    )?;

                    Ok(WorkerFilter::new_and(filters))
                }
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Or(
                    golem_api_grpc::proto::golem::worker::WorkerOrFilter { filters },
                ) => {
                    let filters = filters.into_iter().map(|f| f.try_into()).collect::<Result<
                        Vec<WorkerFilter>,
                        String,
                    >>(
                    )?;

                    Ok(WorkerFilter::new_or(filters))
                }
            },
            None => Err("Missing filter".to_string()),
        }
    }
}

impl From<WorkerFilter> for golem_api_grpc::proto::golem::worker::WorkerFilter {
    fn from(value: WorkerFilter) -> Self {
        let filter = match value {
            WorkerFilter::Name(WorkerNameFilter { comparator, value }) => {
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Name(
                    golem_api_grpc::proto::golem::worker::WorkerNameFilter {
                        comparator: comparator.into(),
                        value,
                    },
                )
            }
            WorkerFilter::Version(WorkerVersionFilter { comparator, value }) => {
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Version(
                    golem_api_grpc::proto::golem::worker::WorkerVersionFilter {
                        comparator: comparator.into(),
                        value,
                    },
                )
            }
            WorkerFilter::Env(WorkerEnvFilter {
                name,
                comparator,
                value,
            }) => golem_api_grpc::proto::golem::worker::worker_filter::Filter::Env(
                golem_api_grpc::proto::golem::worker::WorkerEnvFilter {
                    name,
                    comparator: comparator.into(),
                    value,
                },
            ),
            WorkerFilter::Status(WorkerStatusFilter { comparator, value }) => {
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Status(
                    golem_api_grpc::proto::golem::worker::WorkerStatusFilter {
                        comparator: comparator.into(),
                        value: value.into(),
                    },
                )
            }
            WorkerFilter::CreatedAt(WorkerCreatedAtFilter { comparator, value }) => {
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::CreatedAt(
                    golem_api_grpc::proto::golem::worker::WorkerCreatedAtFilter {
                        value: Some(value.into()),
                        comparator: comparator.into(),
                    },
                )
            }
            WorkerFilter::Not(WorkerNotFilter { filter }) => {
                let f: golem_api_grpc::proto::golem::worker::WorkerFilter = (*filter).into();
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Not(Box::new(
                    golem_api_grpc::proto::golem::worker::WorkerNotFilter {
                        filter: Some(Box::new(f)),
                    },
                ))
            }
            WorkerFilter::And(filter) => {
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::And(
                    golem_api_grpc::proto::golem::worker::WorkerAndFilter {
                        filters: filter.filters.into_iter().map(|f| f.into()).collect(),
                    },
                )
            }
            WorkerFilter::Or(filter) => {
                golem_api_grpc::proto::golem::worker::worker_filter::Filter::Or(
                    golem_api_grpc::proto::golem::worker::WorkerOrFilter {
                        filters: filter.filters.into_iter().map(|f| f.into()).collect(),
                    },
                )
            }
        };

        golem_api_grpc::proto::golem::worker::WorkerFilter {
            filter: Some(filter),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Enum)]
pub enum StringFilterComparator {
    Equal,
    NotEqual,
    Like,
    NotLike,
}

impl StringFilterComparator {
    pub fn matches<T: Display>(&self, value1: &T, value2: &T) -> bool {
        match self {
            StringFilterComparator::Equal => value1.to_string() == value2.to_string(),
            StringFilterComparator::NotEqual => value1.to_string() != value2.to_string(),
            StringFilterComparator::Like => {
                value1.to_string().contains(value2.to_string().as_str())
            }
            StringFilterComparator::NotLike => {
                !value1.to_string().contains(value2.to_string().as_str())
            }
        }
    }
}

impl From<StringFilterComparator> for golem_api_grpc::proto::golem::common::StringFilterComparator {
    fn from(value: StringFilterComparator) -> Self {
        match value {
            StringFilterComparator::Equal => {
                golem_api_grpc::proto::golem::common::StringFilterComparator::StringEqual
            }
            StringFilterComparator::NotEqual => {
                golem_api_grpc::proto::golem::common::StringFilterComparator::StringNotEqual
            }
            StringFilterComparator::Like => {
                golem_api_grpc::proto::golem::common::StringFilterComparator::StringLike
            }
            StringFilterComparator::NotLike => {
                golem_api_grpc::proto::golem::common::StringFilterComparator::StringNotLike
            }
        }
    }
}

impl FromStr for StringFilterComparator {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "==" | "=" | "equal" | "eq" => Ok(StringFilterComparator::Equal),
            "!=" | "notequal" | "ne" => Ok(StringFilterComparator::NotEqual),
            "like" => Ok(StringFilterComparator::Like),
            "notlike" => Ok(StringFilterComparator::NotLike),
            _ => Err(format!("Unknown String Filter Comparator: {}", s)),
        }
    }
}

impl TryFrom<i32> for StringFilterComparator {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(StringFilterComparator::Equal),
            1 => Ok(StringFilterComparator::NotEqual),
            2 => Ok(StringFilterComparator::Like),
            3 => Ok(StringFilterComparator::NotLike),
            _ => Err(format!("Unknown String Filter Comparator: {}", value)),
        }
    }
}

impl From<StringFilterComparator> for i32 {
    fn from(value: StringFilterComparator) -> Self {
        match value {
            StringFilterComparator::Equal => 0,
            StringFilterComparator::NotEqual => 1,
            StringFilterComparator::Like => 2,
            StringFilterComparator::NotLike => 3,
        }
    }
}

impl Display for StringFilterComparator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StringFilterComparator::Equal => "==",
            StringFilterComparator::NotEqual => "!=",
            StringFilterComparator::Like => "like",
            StringFilterComparator::NotLike => "notlike",
        };
        write!(f, "{}", s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode, Enum)]
pub enum FilterComparator {
    Equal,
    NotEqual,
    GreaterEqual,
    Greater,
    LessEqual,
    Less,
}

impl Display for FilterComparator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FilterComparator::Equal => "==",
            FilterComparator::NotEqual => "!=",
            FilterComparator::GreaterEqual => ">=",
            FilterComparator::Greater => ">",
            FilterComparator::LessEqual => "<=",
            FilterComparator::Less => "<",
        };
        write!(f, "{}", s)
    }
}

impl FilterComparator {
    pub fn matches<T: Ord>(&self, value1: &T, value2: &T) -> bool {
        match self {
            FilterComparator::Equal => value1 == value2,
            FilterComparator::NotEqual => value1 != value2,
            FilterComparator::Less => value1 < value2,
            FilterComparator::LessEqual => value1 <= value2,
            FilterComparator::Greater => value1 > value2,
            FilterComparator::GreaterEqual => value1 >= value2,
        }
    }
}

impl FromStr for FilterComparator {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "==" | "=" | "equal" | "eq" => Ok(FilterComparator::Equal),
            "!=" | "notequal" | "ne" => Ok(FilterComparator::NotEqual),
            ">=" | "greaterequal" | "ge" => Ok(FilterComparator::GreaterEqual),
            ">" | "greater" | "gt" => Ok(FilterComparator::Greater),
            "<=" | "lessequal" | "le" => Ok(FilterComparator::LessEqual),
            "<" | "less" | "lt" => Ok(FilterComparator::Less),
            _ => Err(format!("Unknown Filter Comparator: {}", s)),
        }
    }
}

impl From<FilterComparator> for golem_api_grpc::proto::golem::common::FilterComparator {
    fn from(value: FilterComparator) -> Self {
        match value {
            FilterComparator::Equal => {
                golem_api_grpc::proto::golem::common::FilterComparator::Equal
            }
            FilterComparator::NotEqual => {
                golem_api_grpc::proto::golem::common::FilterComparator::NotEqual
            }
            FilterComparator::Less => golem_api_grpc::proto::golem::common::FilterComparator::Less,
            FilterComparator::LessEqual => {
                golem_api_grpc::proto::golem::common::FilterComparator::LessEqual
            }
            FilterComparator::Greater => {
                golem_api_grpc::proto::golem::common::FilterComparator::Greater
            }
            FilterComparator::GreaterEqual => {
                golem_api_grpc::proto::golem::common::FilterComparator::GreaterEqual
            }
        }
    }
}

impl TryFrom<i32> for FilterComparator {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FilterComparator::Equal),
            1 => Ok(FilterComparator::NotEqual),
            2 => Ok(FilterComparator::Less),
            3 => Ok(FilterComparator::LessEqual),
            4 => Ok(FilterComparator::Greater),
            5 => Ok(FilterComparator::GreaterEqual),
            _ => Err(format!("Unknown Filter Comparator: {}", value)),
        }
    }
}

impl From<FilterComparator> for i32 {
    fn from(value: FilterComparator) -> Self {
        match value {
            FilterComparator::Equal => 0,
            FilterComparator::NotEqual => 1,
            FilterComparator::Less => 2,
            FilterComparator::LessEqual => 3,
            FilterComparator::Greater => 4,
            FilterComparator::GreaterEqual => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Object, Default)]
pub struct ScanCursor {
    pub cursor: u64,
    pub layer: usize,
}

impl ScanCursor {
    pub fn is_finished(&self) -> bool {
        self.cursor == 0
    }
}

impl Display for ScanCursor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.layer, self.cursor)
    }
}

impl FromStr for ScanCursor {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts = s.split('/').collect::<Vec<&str>>();
        if parts.len() == 2 {
            Ok(ScanCursor {
                layer: parts[0]
                    .parse()
                    .map_err(|e| format!("Invalid layer part: {}", e))?,
                cursor: parts[1]
                    .parse()
                    .map_err(|e| format!("Invalid cursor part: {}", e))?,
            })
        } else {
            Err("Invalid cursor, must have 'layer/cursor' format".to_string())
        }
    }
}

impl From<Cursor> for ScanCursor {
    fn from(value: Cursor) -> Self {
        Self {
            cursor: value.cursor,
            layer: value.layer as usize,
        }
    }
}

impl From<ScanCursor> for Cursor {
    fn from(value: ScanCursor) -> Self {
        Self {
            cursor: value.cursor,
            layer: value.layer as u64,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Critical,
}

impl From<golem_api_grpc::proto::golem::worker::Level> for LogLevel {
    fn from(value: golem_api_grpc::proto::golem::worker::Level) -> Self {
        match value {
            golem_api_grpc::proto::golem::worker::Level::Trace => LogLevel::Trace,
            golem_api_grpc::proto::golem::worker::Level::Debug => LogLevel::Debug,
            golem_api_grpc::proto::golem::worker::Level::Info => LogLevel::Info,
            golem_api_grpc::proto::golem::worker::Level::Warn => LogLevel::Warn,
            golem_api_grpc::proto::golem::worker::Level::Error => LogLevel::Error,
            golem_api_grpc::proto::golem::worker::Level::Critical => LogLevel::Critical,
        }
    }
}

impl From<LogLevel> for golem_api_grpc::proto::golem::worker::Level {
    fn from(value: LogLevel) -> Self {
        match value {
            LogLevel::Trace => golem_api_grpc::proto::golem::worker::Level::Trace,
            LogLevel::Debug => golem_api_grpc::proto::golem::worker::Level::Debug,
            LogLevel::Info => golem_api_grpc::proto::golem::worker::Level::Info,
            LogLevel::Warn => golem_api_grpc::proto::golem::worker::Level::Warn,
            LogLevel::Error => golem_api_grpc::proto::golem::worker::Level::Error,
            LogLevel::Critical => golem_api_grpc::proto::golem::worker::Level::Critical,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkerEvent {
    StdOut {
        timestamp: Timestamp,
        bytes: Vec<u8>,
    },
    StdErr {
        timestamp: Timestamp,
        bytes: Vec<u8>,
    },
    Log {
        timestamp: Timestamp,
        level: LogLevel,
        context: String,
        message: String,
    },
    InvocationStart {
        timestamp: Timestamp,
        function: String,
        idempotency_key: IdempotencyKey,
    },
    InvocationFinished {
        timestamp: Timestamp,
        function: String,
        idempotency_key: IdempotencyKey,
    },
    Close,
}

impl WorkerEvent {
    pub fn stdout(bytes: Vec<u8>) -> WorkerEvent {
        WorkerEvent::StdOut {
            timestamp: Timestamp::now_utc(),
            bytes,
        }
    }

    pub fn stderr(bytes: Vec<u8>) -> WorkerEvent {
        WorkerEvent::StdErr {
            timestamp: Timestamp::now_utc(),
            bytes,
        }
    }

    pub fn log(level: LogLevel, context: &str, message: &str) -> WorkerEvent {
        WorkerEvent::Log {
            timestamp: Timestamp::now_utc(),
            level,
            context: context.to_string(),
            message: message.to_string(),
        }
    }

    pub fn invocation_start(function: &str, idempotency_key: &IdempotencyKey) -> WorkerEvent {
        WorkerEvent::InvocationStart {
            timestamp: Timestamp::now_utc(),
            function: function.to_string(),
            idempotency_key: idempotency_key.clone(),
        }
    }

    pub fn invocation_finished(function: &str, idempotency_key: &IdempotencyKey) -> WorkerEvent {
        WorkerEvent::InvocationFinished {
            timestamp: Timestamp::now_utc(),
            function: function.to_string(),
            idempotency_key: idempotency_key.clone(),
        }
    }

    pub fn as_oplog_entry(&self) -> Option<OplogEntry> {
        match self {
            WorkerEvent::StdOut { timestamp, bytes } => Some(OplogEntry::Log {
                timestamp: *timestamp,
                level: oplog::LogLevel::Stdout,
                context: String::new(),
                message: String::from_utf8_lossy(bytes).to_string(),
            }),
            WorkerEvent::StdErr { timestamp, bytes } => Some(OplogEntry::Log {
                timestamp: *timestamp,
                level: oplog::LogLevel::Stderr,
                context: String::new(),
                message: String::from_utf8_lossy(bytes).to_string(),
            }),
            WorkerEvent::Log {
                timestamp,
                level,
                context,
                message,
            } => Some(OplogEntry::Log {
                timestamp: *timestamp,
                level: match level {
                    LogLevel::Trace => oplog::LogLevel::Trace,
                    LogLevel::Debug => oplog::LogLevel::Debug,
                    LogLevel::Info => oplog::LogLevel::Info,
                    LogLevel::Warn => oplog::LogLevel::Warn,
                    LogLevel::Error => oplog::LogLevel::Error,
                    LogLevel::Critical => oplog::LogLevel::Critical,
                },
                context: context.clone(),
                message: message.clone(),
            }),
            WorkerEvent::InvocationStart { .. } => None,
            WorkerEvent::InvocationFinished { .. } => None,
            WorkerEvent::Close => None,
        }
    }
}

impl Display for WorkerEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerEvent::StdOut { bytes, .. } => {
                write!(
                    f,
                    "<stdout> {}",
                    String::from_utf8(bytes.clone()).unwrap_or_default()
                )
            }
            WorkerEvent::StdErr { bytes, .. } => {
                write!(
                    f,
                    "<stderr> {}",
                    String::from_utf8(bytes.clone()).unwrap_or_default()
                )
            }
            WorkerEvent::Log {
                level,
                context,
                message,
                ..
            } => {
                write!(f, "<log> {:?} {} {}", level, context, message)
            }
            WorkerEvent::InvocationStart {
                function,
                idempotency_key,
                ..
            } => {
                write!(f, "<invocation-start> {} {}", function, idempotency_key)
            }
            WorkerEvent::InvocationFinished {
                function,
                idempotency_key,
                ..
            } => {
                write!(f, "<invocation-finished> {} {}", function, idempotency_key)
            }
            WorkerEvent::Close => {
                write!(f, "<close>")
            }
        }
    }
}

impl TryFrom<golem_api_grpc::proto::golem::worker::LogEvent> for WorkerEvent {
    type Error = String;

    fn try_from(
        value: golem_api_grpc::proto::golem::worker::LogEvent,
    ) -> Result<Self, Self::Error> {
        match value.event {
            Some(event) => match event {
                golem_api_grpc::proto::golem::worker::log_event::Event::Stdout(event) => {
                    Ok(WorkerEvent::StdOut {
                        timestamp: event.timestamp.ok_or("Missing timestamp")?.into(),
                        bytes: event.message.into_bytes(),
                    })
                }
                golem_api_grpc::proto::golem::worker::log_event::Event::Stderr(event) => {
                    Ok(WorkerEvent::StdErr {
                        timestamp: event.timestamp.ok_or("Missing timestamp")?.into(),
                        bytes: event.message.into_bytes(),
                    })
                }
                golem_api_grpc::proto::golem::worker::log_event::Event::Log(event) => {
                    Ok(WorkerEvent::Log {
                        timestamp: event.timestamp.clone().ok_or("Missing timestamp")?.into(),
                        level: event.level().into(),
                        context: event.context,
                        message: event.message,
                    })
                }
                golem_api_grpc::proto::golem::worker::log_event::Event::InvocationStarted(
                    event,
                ) => Ok(WorkerEvent::InvocationStart {
                    timestamp: event.timestamp.ok_or("Missing timestamp")?.into(),
                    function: event.function,
                    idempotency_key: event
                        .idempotency_key
                        .ok_or("Missing idempotency key")?
                        .into(),
                }),
                golem_api_grpc::proto::golem::worker::log_event::Event::InvocationFinished(
                    event,
                ) => Ok(WorkerEvent::InvocationFinished {
                    timestamp: event.timestamp.ok_or("Missing timestamp")?.into(),
                    function: event.function,
                    idempotency_key: event
                        .idempotency_key
                        .ok_or("Missing idempotency key")?
                        .into(),
                }),
            },
            None => Err("Missing event".to_string()),
        }
    }
}

impl TryFrom<WorkerEvent> for golem_api_grpc::proto::golem::worker::LogEvent {
    type Error = String;

    fn try_from(value: WorkerEvent) -> Result<Self, Self::Error> {
        match value {
            WorkerEvent::StdOut { timestamp, bytes } => Ok(golem::worker::LogEvent {
                event: Some(golem::worker::log_event::Event::Stdout(
                    golem::worker::StdOutLog {
                        message: String::from_utf8_lossy(&bytes).to_string(),
                        timestamp: Some(timestamp.into()),
                    },
                )),
            }),
            WorkerEvent::StdErr { timestamp, bytes } => Ok(golem::worker::LogEvent {
                event: Some(
                    golem_api_grpc::proto::golem::worker::log_event::Event::Stderr(
                        golem::worker::StdErrLog {
                            message: String::from_utf8_lossy(&bytes).to_string(),
                            timestamp: Some(timestamp.into()),
                        },
                    ),
                ),
            }),
            WorkerEvent::Log {
                timestamp,
                level,
                context,
                message,
            } => Ok(golem::worker::LogEvent {
                event: Some(golem::worker::log_event::Event::Log(golem::worker::Log {
                    level: match level {
                        LogLevel::Trace => golem::worker::Level::Trace.into(),
                        LogLevel::Debug => golem::worker::Level::Debug.into(),
                        LogLevel::Info => golem::worker::Level::Info.into(),
                        LogLevel::Warn => golem::worker::Level::Warn.into(),
                        LogLevel::Error => golem::worker::Level::Error.into(),
                        LogLevel::Critical => golem::worker::Level::Critical.into(),
                    },
                    context,
                    message,
                    timestamp: Some(timestamp.into()),
                })),
            }),
            WorkerEvent::InvocationStart {
                timestamp,
                function,
                idempotency_key,
            } => Ok(golem::worker::LogEvent {
                event: Some(golem::worker::log_event::Event::InvocationStarted(
                    golem::worker::InvocationStarted {
                        function,
                        idempotency_key: Some(idempotency_key.into()),
                        timestamp: Some(timestamp.into()),
                    },
                )),
            }),
            WorkerEvent::InvocationFinished {
                timestamp,
                function,
                idempotency_key,
            } => Ok(golem::worker::LogEvent {
                event: Some(golem::worker::log_event::Event::InvocationFinished(
                    golem::worker::InvocationFinished {
                        function,
                        idempotency_key: Some(idempotency_key.into()),
                        timestamp: Some(timestamp.into()),
                    },
                )),
            }),
            WorkerEvent::Close => Err("Close event is not supported via protobuf".to_string()),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Enum)]
#[repr(i32)]
pub enum ComponentType {
    Durable = 0,
    Ephemeral = 1,
}

impl TryFrom<i32> for ComponentType {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ComponentType::Durable),
            1 => Ok(ComponentType::Ephemeral),
            _ => Err(format!("Unknown Component Type: {}", value)),
        }
    }
}

impl From<golem_api_grpc::proto::golem::component::ComponentType> for ComponentType {
    fn from(value: golem_api_grpc::proto::golem::component::ComponentType) -> Self {
        match value {
            golem_api_grpc::proto::golem::component::ComponentType::Durable => {
                ComponentType::Durable
            }
            golem_api_grpc::proto::golem::component::ComponentType::Ephemeral => {
                ComponentType::Ephemeral
            }
        }
    }
}

impl From<ComponentType> for golem_api_grpc::proto::golem::component::ComponentType {
    fn from(value: ComponentType) -> Self {
        match value {
            ComponentType::Durable => {
                golem_api_grpc::proto::golem::component::ComponentType::Durable
            }
            ComponentType::Ephemeral => {
                golem_api_grpc::proto::golem::component::ComponentType::Ephemeral
            }
        }
    }
}

impl Display for ComponentType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ComponentType::Durable => "Durable",
            ComponentType::Ephemeral => "Ephemeral",
        };
        write!(f, "{}", s)
    }
}

impl FromStr for ComponentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Durable" => Ok(ComponentType::Durable),
            "Ephemeral" => Ok(ComponentType::Ephemeral),
            _ => Err(format!("Unknown Component Type: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use test_r::test;

    use std::collections::HashSet;
    use std::str::FromStr;
    use std::time::SystemTime;
    use std::vec;

    use crate::model::oplog::OplogIndex;
    use crate::model::{
        AccountId, ComponentId, FilterComparator, IdempotencyKey, ShardId, StringFilterComparator,
        TargetWorkerId, Timestamp, WorkerFilter, WorkerId, WorkerMetadata, WorkerStatus,
        WorkerStatusRecord,
    };
    use bincode::{Decode, Encode};
    use poem_openapi::types::ToJSON;
    use rand::{thread_rng, Rng};
    use serde::{Deserialize, Serialize};

    #[test]
    fn timestamp_conversion() {
        let ts: Timestamp = Timestamp::now_utc();

        let prost_ts: prost_types::Timestamp = ts.into();

        let ts2: Timestamp = prost_ts.into();

        assert_eq!(ts2, ts);
    }

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
    struct ExampleWithAccountId {
        account_id: AccountId,
    }

    #[test]
    fn account_id_from_json_apigateway_version() {
        let json = "{ \"account_id\": \"account-1\" }";
        let example: ExampleWithAccountId = serde_json::from_str(json).unwrap();
        assert_eq!(
            example.account_id,
            AccountId {
                value: "account-1".to_string()
            }
        );
    }

    #[test]
    fn account_id_json_serialization() {
        // We want to use this variant for serialization because it is used on the public API gateway API
        let example: ExampleWithAccountId = ExampleWithAccountId {
            account_id: AccountId {
                value: "account-1".to_string(),
            },
        };
        let json = serde_json::to_string(&example).unwrap();
        assert_eq!(json, "{\"account_id\":\"account-1\"}");
    }

    #[test]
    fn worker_filter_parse() {
        assert_eq!(
            WorkerFilter::from_str(" name =  worker-1").unwrap(),
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string())
        );

        assert_eq!(
            WorkerFilter::from_str("status == Running").unwrap(),
            WorkerFilter::new_status(FilterComparator::Equal, WorkerStatus::Running)
        );

        assert_eq!(
            WorkerFilter::from_str("version >= 10").unwrap(),
            WorkerFilter::new_version(FilterComparator::GreaterEqual, 10)
        );

        assert_eq!(
            WorkerFilter::from_str("env.tag1 == abc ").unwrap(),
            WorkerFilter::new_env(
                "tag1".to_string(),
                StringFilterComparator::Equal,
                "abc".to_string(),
            )
        );
    }

    #[test]
    fn worker_filter_combination() {
        assert_eq!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()).not(),
            WorkerFilter::new_not(WorkerFilter::new_name(
                StringFilterComparator::Equal,
                "worker-1".to_string(),
            ))
        );

        assert_eq!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()).and(
                WorkerFilter::new_status(FilterComparator::Equal, WorkerStatus::Running)
            ),
            WorkerFilter::new_and(vec![
                WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()),
                WorkerFilter::new_status(FilterComparator::Equal, WorkerStatus::Running),
            ])
        );

        assert_eq!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string())
                .and(WorkerFilter::new_status(
                    FilterComparator::Equal,
                    WorkerStatus::Running,
                ))
                .and(WorkerFilter::new_version(FilterComparator::Equal, 1)),
            WorkerFilter::new_and(vec![
                WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()),
                WorkerFilter::new_status(FilterComparator::Equal, WorkerStatus::Running),
                WorkerFilter::new_version(FilterComparator::Equal, 1),
            ])
        );

        assert_eq!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()).or(
                WorkerFilter::new_status(FilterComparator::Equal, WorkerStatus::Running)
            ),
            WorkerFilter::new_or(vec![
                WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()),
                WorkerFilter::new_status(FilterComparator::Equal, WorkerStatus::Running),
            ])
        );

        assert_eq!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string())
                .or(WorkerFilter::new_status(
                    FilterComparator::NotEqual,
                    WorkerStatus::Running,
                ))
                .or(WorkerFilter::new_version(FilterComparator::Equal, 1)),
            WorkerFilter::new_or(vec![
                WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()),
                WorkerFilter::new_status(FilterComparator::NotEqual, WorkerStatus::Running),
                WorkerFilter::new_version(FilterComparator::Equal, 1),
            ])
        );

        assert_eq!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string())
                .and(WorkerFilter::new_status(
                    FilterComparator::NotEqual,
                    WorkerStatus::Running,
                ))
                .or(WorkerFilter::new_version(FilterComparator::Equal, 1)),
            WorkerFilter::new_or(vec![
                WorkerFilter::new_and(vec![
                    WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()),
                    WorkerFilter::new_status(FilterComparator::NotEqual, WorkerStatus::Running),
                ]),
                WorkerFilter::new_version(FilterComparator::Equal, 1),
            ])
        );

        assert_eq!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string())
                .or(WorkerFilter::new_status(
                    FilterComparator::NotEqual,
                    WorkerStatus::Running,
                ))
                .and(WorkerFilter::new_version(FilterComparator::Equal, 1)),
            WorkerFilter::new_and(vec![
                WorkerFilter::new_or(vec![
                    WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string()),
                    WorkerFilter::new_status(FilterComparator::NotEqual, WorkerStatus::Running),
                ]),
                WorkerFilter::new_version(FilterComparator::Equal, 1),
            ])
        );
    }

    #[test]
    fn worker_filter_matches() {
        let component_id = ComponentId::new_v4();
        let worker_metadata = WorkerMetadata {
            worker_id: WorkerId {
                worker_name: "worker-1".to_string(),
                component_id,
            },
            args: vec![],
            env: vec![
                ("env1".to_string(), "value1".to_string()),
                ("env2".to_string(), "value2".to_string()),
            ],
            account_id: AccountId {
                value: "account-1".to_string(),
            },
            created_at: Timestamp::now_utc(),
            parent: None,
            last_known_status: WorkerStatusRecord {
                component_version: 1,
                ..WorkerStatusRecord::default()
            },
        };

        assert!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string())
                .and(WorkerFilter::new_status(
                    FilterComparator::Equal,
                    WorkerStatus::Idle,
                ))
                .matches(&worker_metadata)
        );

        assert!(WorkerFilter::new_env(
            "env1".to_string(),
            StringFilterComparator::Equal,
            "value1".to_string(),
        )
        .and(WorkerFilter::new_status(
            FilterComparator::Equal,
            WorkerStatus::Idle,
        ))
        .matches(&worker_metadata));

        assert!(WorkerFilter::new_env(
            "env1".to_string(),
            StringFilterComparator::Equal,
            "value2".to_string(),
        )
        .not()
        .and(
            WorkerFilter::new_status(FilterComparator::Equal, WorkerStatus::Running).or(
                WorkerFilter::new_status(FilterComparator::Equal, WorkerStatus::Idle)
            )
        )
        .matches(&worker_metadata));

        assert!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-1".to_string())
                .and(WorkerFilter::new_version(FilterComparator::Equal, 1))
                .matches(&worker_metadata)
        );

        assert!(
            WorkerFilter::new_name(StringFilterComparator::Equal, "worker-2".to_string())
                .or(WorkerFilter::new_version(FilterComparator::Equal, 1))
                .matches(&worker_metadata)
        );

        assert!(WorkerFilter::new_version(FilterComparator::GreaterEqual, 1)
            .and(WorkerFilter::new_version(FilterComparator::Less, 2))
            .or(WorkerFilter::new_name(
                StringFilterComparator::Equal,
                "worker-2".to_string(),
            ))
            .matches(&worker_metadata));
    }

    #[test]
    fn target_worker_id_force_shards() {
        let mut rng = thread_rng();
        const SHARD_COUNT: usize = 1000;
        const EXAMPLE_COUNT: usize = 1000;
        for _ in 0..EXAMPLE_COUNT {
            let mut shard_ids = HashSet::new();
            let count = rng.gen_range(0..100);
            for _ in 0..count {
                let shard_id = rng.gen_range(0..SHARD_COUNT);
                shard_ids.insert(ShardId {
                    value: shard_id as i64,
                });
            }

            let component_id = ComponentId::new_v4();
            let target_worker_id = TargetWorkerId {
                component_id,
                worker_name: None,
            };

            let start = SystemTime::now();
            let worker_id = target_worker_id.into_worker_id(&shard_ids, SHARD_COUNT);
            let end = SystemTime::now();
            println!(
                "Time with {count} valid shards: {:?}",
                end.duration_since(start).unwrap()
            );

            if !shard_ids.is_empty() {
                assert!(shard_ids.contains(&ShardId::from_worker_id(&worker_id, SHARD_COUNT)));
            }
        }
    }

    #[test]
    fn derived_idempotency_key() {
        let base1 = IdempotencyKey::fresh();
        let base2 = IdempotencyKey::fresh();
        let base3 = IdempotencyKey {
            value: "base3".to_string(),
        };

        assert_ne!(base1, base2);

        let idx1 = OplogIndex::from_u64(2);
        let idx2 = OplogIndex::from_u64(11);

        let derived11a = IdempotencyKey::derived(&base1, idx1);
        let derived12a = IdempotencyKey::derived(&base1, idx2);
        let derived21a = IdempotencyKey::derived(&base2, idx1);
        let derived22a = IdempotencyKey::derived(&base2, idx2);

        let derived11b = IdempotencyKey::derived(&base1, idx1);
        let derived12b = IdempotencyKey::derived(&base1, idx2);
        let derived21b = IdempotencyKey::derived(&base2, idx1);
        let derived22b = IdempotencyKey::derived(&base2, idx2);

        let derived31 = IdempotencyKey::derived(&base3, idx1);
        let derived32 = IdempotencyKey::derived(&base3, idx2);

        assert_eq!(derived11a, derived11b);
        assert_eq!(derived12a, derived12b);
        assert_eq!(derived21a, derived21b);
        assert_eq!(derived22a, derived22b);

        assert_ne!(derived11a, derived12a);
        assert_ne!(derived11a, derived21a);
        assert_ne!(derived11a, derived22a);
        assert_ne!(derived12a, derived21a);
        assert_ne!(derived12a, derived22a);
        assert_ne!(derived21a, derived22a);

        assert_ne!(derived11a, derived31);
        assert_ne!(derived21a, derived31);
        assert_ne!(derived12a, derived32);
        assert_ne!(derived22a, derived32);
        assert_ne!(derived31, derived32);
    }

    #[test]
    fn worker_status_serialization_poem_serde_equivalence() {
        let status = WorkerStatus::Retrying;
        let serialized = status.to_json_string();
        let deserialized: WorkerStatus = serde_json::from_str(&serialized).unwrap();
        assert_eq!(status, deserialized);
    }

    #[test]
    fn idempotency_key_serialization_poem_serde_equivalence() {
        let key = IdempotencyKey::fresh();
        let serialized = key.to_json_string();
        let deserialized: IdempotencyKey = serde_json::from_str(&serialized).unwrap();
        assert_eq!(key, deserialized);
    }
}
