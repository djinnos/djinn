apply_patch(*** Begin Patch
*** Update File: server/crates/djinn-agent/src/actors/slot/reply_loop/mod.rs
@@                            rmcp::model::ResourceContents::TextResourceContents {
                                uri,
                                mime_type,
                                text,
                                ..
                            } => {
                                out.push_str(&format!("Resource: {uri}\n"));
                                if let Some(mime) = mime_type {
                                    out.push_str(&format!("MIME: {mime}\n"));
                                }
                                out.push_str(text);
                            }
                            rmcp::model::ResourceContents::BlobResourceContents {
                                uri,
                                mime_type,
                                ..
                            } => {
                                out.push_str(&format!(
                                    "Resource: {uri}\nMIME: {}\n[binary resource omitted]",
                                    mime_type.as_deref().unwrap_or("application/octet-stream")
                                ));
                            }
*** End Patch)