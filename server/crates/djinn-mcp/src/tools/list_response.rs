use serde::Serialize;

#[derive(Serialize, schemars::JsonSchema)]
pub struct NamedListResponse<T> {
    #[schemars(rename = "tasks")]
    pub items: Vec<T>,
    pub total_count: i64,
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

impl<T> NamedListResponse<T> {
    pub fn empty(limit: i64, offset: i64) -> Self {
        Self {
            items: Vec::new(),
            total_count: 0,
            limit,
            offset,
            has_more: false,
        }
    }

    pub fn new(items: Vec<T>, total_count: i64, limit: i64, offset: i64) -> Self {
        Self {
            has_more: offset + limit < total_count,
            items,
            total_count,
            limit,
            offset,
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ErrorNamedListResponse<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(rename = "tasks")]
    pub items: Option<Vec<T>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T> ErrorNamedListResponse<T> {
    pub fn ok(items: Vec<T>, total_count: i64, limit: i64, offset: i64) -> Self {
        Self {
            items: Some(items),
            total_count: Some(total_count),
            limit: Some(limit),
            offset: Some(offset),
            has_more: Some(offset + limit < total_count),
            error: None,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            items: None,
            total_count: None,
            limit: None,
            offset: None,
            has_more: None,
            error: Some(error.into()),
        }
    }
}
