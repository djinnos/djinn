//! Extension context capability trait.
//!
//! This module will define the `ExtensionContext` trait that provides a
//! narrow capability interface for extension handlers, replacing direct
//! dependency on the concrete `AgentContext` god struct. Handlers in
//! `dispatch` will be generic over this trait rather than importing
//! `djinn-agent` internals.
