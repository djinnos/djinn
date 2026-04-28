//! AST-aware chunker types. PR B1 ships only the public row shape so the
//! repository layer + migration land independently of B2's chunking logic.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeChunk {
    pub id: String,
    pub project_id: String,
    pub file_path: String,
    pub symbol_key: Option<String>,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: String,
    pub embedded_text: String,
}
