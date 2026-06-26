//! Wiki knowledge management — compute and orchestration layer.
//!
//! This module owns the "business logic" half of the wiki system:
//! - Ingest pipeline (parsing, LLM extraction, dedup)
//! - Structural lint and auto-fix
//! - Background indexing service (broker consumer)
//!
//! Data types live in `gasket_storage::wiki` alongside their persistence layer.
//! Persistence (PageStore, PageIndex) lives in `gasket_storage::wiki`.

pub mod indexing_service;
pub mod ingest;
pub mod lint;

// ── Re-exports from gasket-storage (data + persistence) ─────
pub use gasket_storage::wiki::{
    create_wiki_tables, slugify, Frequency, PageFilter, PageIndex, PageStore, PageSummary,
    PageType, WikiPage,
};

// ── Re-exports from internal compute modules ─────────────────
pub use indexing_service::{
    WikiEmbeddingProvider, WikiIndexingService, WikiVectorHit, WikiVectorStore,
};
pub use ingest::{
    ConversationParser, DedupResult, ExtractedItem, ExtractedItemType, ExtractionResult,
    HtmlParser, KnowledgeExtractor, MarkdownParser, ParsedSource, PlainTextParser,
    SemanticDeduplicator, SourceFormat, SourceMetadata, SourceParser,
};
pub use lint::{
    extract_page_references, FixReport, LintReport, Severity, StructuralIssue, StructuralIssueType,
    StructuralLintConfig, WikiLinter,
};
