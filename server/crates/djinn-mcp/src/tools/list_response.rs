use rmcp::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

#[derive(Clone, Debug)]
pub struct ListMeta {
    pub total_count: Option<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub has_more: Option<bool>,
    pub error: Option<String>,
}

impl ListMeta {
    pub fn success(total_count: i64, limit: i64, offset: i64) -> Self {
        Self {
            total_count: Some(total_count),
            limit: Some(limit),
            offset: Some(offset),
            has_more: Some(offset + limit < total_count),
            error: None,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            total_count: None,
            limit: None,
            offset: None,
            has_more: None,
            error: Some(error.into()),
        }
    }

    pub fn empty(limit: i64, offset: i64) -> Self {
        Self {
            total_count: Some(0),
            limit: Some(limit),
            offset: Some(offset),
            has_more: Some(false),
            error: None,
        }
    }
}

pub trait NamedListResponse: Sized {
    type Item;

    const FIELD_NAME: &'static str;
    const TITLE: &'static str;

    fn from_parts(items: Option<Vec<Self::Item>>, meta: ListMeta) -> Self;
    fn items(&self) -> Option<&Vec<Self::Item>>;
    fn meta(&self) -> &ListMeta;
}

pub fn success<R>(items: Vec<R::Item>, total_count: i64, limit: i64, offset: i64) -> R
where
    R: NamedListResponse,
{
    R::from_parts(Some(items), ListMeta::success(total_count, limit, offset))
}

pub fn error<R>(error: impl Into<String>) -> R
where
    R: NamedListResponse,
{
    R::from_parts(None, ListMeta::error(error))
}

pub fn empty<R>(items: Vec<R::Item>, limit: i64, offset: i64) -> R
where
    R: NamedListResponse,
{
    R::from_parts(Some(items), ListMeta::empty(limit, offset))
}

pub fn serialize_named_list_response<R, S>(response: &R, serializer: S) -> Result<S::Ok, S::Error>
where
    R: NamedListResponse,
    R::Item: Serialize,
    S: Serializer,
{
    let meta = response.meta();
    let mut map = serializer.serialize_map(None)?;

    if let Some(items) = response.items() {
        map.serialize_entry(R::FIELD_NAME, items)?;
    }
    if let Some(total_count) = meta.total_count {
        map.serialize_entry("total_count", &total_count)?;
    }
    if let Some(limit) = meta.limit {
        map.serialize_entry("limit", &limit)?;
    }
    if let Some(offset) = meta.offset {
        map.serialize_entry("offset", &offset)?;
    }
    if let Some(has_more) = meta.has_more {
        map.serialize_entry("has_more", &has_more)?;
    }
    if let Some(error) = &meta.error {
        map.serialize_entry("error", error)?;
    }

    map.end()
}

pub fn named_list_response_schema<Item>(
    generator: &mut SchemaGenerator,
    title: &str,
    field_name: &str,
) -> Schema
where
    Item: JsonSchema,
{
    json_schema!({
        "title": title,
        "type": "object",
        "properties": {
            field_name: generator.subschema_for::<Option<Vec<Item>>>(),
            "total_count": generator.subschema_for::<Option<i64>>(),
            "limit": generator.subschema_for::<Option<i64>>(),
            "offset": generator.subschema_for::<Option<i64>>(),
            "has_more": generator.subschema_for::<Option<bool>>(),
            "error": generator.subschema_for::<Option<String>>()
        }
    })
}
