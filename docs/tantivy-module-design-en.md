# Tantivy Module Design Document

## 1. Overview

**`gasket-tantivy`** is a CLI tool for managing Tantivy full-text search indexes.

**Core Responsibilities:**
- Creating, managing, and querying multiple independent indexes
- Full-text search with BM25, filtering, and highlighting
- Index maintenance operations (backup, compaction, rebuild, expiration)
- Process-level file locking to prevent concurrent access
- Default storage path: `~/.gasket/tantivy/`

---

## 2. Directory Structure

```
gasket/tantivy/
├── src/
│   ├── lib.rs                      # Library root (exports public API)
│   ├── main.rs                     # CLI entry point
│   ├── error.rs                    # Error types
│   ├── index/
│   │   ├── mod.rs                  # Index module exports
│   │   ├── document.rs             # Document, BatchDocumentInput types
│   │   ├── schema.rs               # Schema definitions (FieldDef, FieldType, IndexSchema)
│   │   ├── manager.rs              # IndexManager (core index operations)
│   │   ├── search.rs               # SearchQuery, SearchResult types
│   │   └── lock.rs                 # File-based IndexLock
│   └── maintenance/
│       ├── mod.rs                  # Maintenance module exports
│       ├── backup.rs               # Backup/restore operations
│       ├── compact.rs              # Compaction operations
│       ├── expire.rs               # TTL/document expiration
│       ├── rebuild.rs              # Index rebuild with schema migration
│       └── stats.rs                # IndexHealth status
└── tests/
    └── integration_test.rs
```

---

## 3. CLI Commands

### Index Management
```bash
index create --name <name> --fields <json-array> [--default-ttl <duration>]
index list
index stats [--name <name>]
index drop --name <name>
index compact --name <name>
index rebuild --name <name> [--fields <json-array>]
```

### Document Operations
```bash
doc add --index <name> --id <id> --fields <json-object> [--ttl <duration>]
doc add-batch --index <name> (--file <path> | --documents <json>) [--ttl <duration>] [--parallel <n>]
doc delete --index <name> --id <id>
doc commit --index <name>
```

### Search
```bash
search --index <name> --query <json-query>
```

**SearchQuery JSON Format:**
```json
{
  "text": "search keywords",
  "filters": [{"field": "status", "op": "eq", "value": "active"}],
  "limit": 10,
  "offset": 0,
  "highlight": {"fields": ["title"], "highlight_tag": "mark"}
}
```

---

## 4. Core Data Types

### 4.1 Schema Types (index/schema.rs)

**FieldType** - Supported field types for indexing:
- `Text` - Full-text indexed (tokenized for BM25)
- `String` - Exact match (not tokenized)
- `I64` / `F64` - Numeric fields
- `DateTime` - ISO 8601 timestamps
- `StringArray` - Multiple string values (tags, labels)
- `Json` - Stored-only JSON

**FieldDef** - Field definition:
```rust
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub indexed: bool,  // Include in search index
    pub stored: bool,   // Return in search results
}
```

### 4.2 Document Types (index/document.rs)

```rust
pub struct Document {
    pub id: String,
    pub fields: Map<String, Value>,
    pub expires_at: Option<DateTime<Utc>>,  // TTL support
}
```

### 4.3 Search Types (index/search.rs)

```rust
pub struct SearchQuery {
    pub text: Option<String>,              // Full-text search
    pub filters: Vec<FieldFilter>,         // Field filters
    pub limit: usize,                      // Max results (default: 10)
    pub offset: usize,                      // Pagination offset
    pub sort: Option<SortConfig>,          // Sort configuration
    pub highlight: Option<HighlightConfig>, // Highlighting
}

pub struct SearchResult {
    pub id: String,
    pub fields: Map<String, Value>,
    pub score: f32,
    pub highlights: Option<Map<String, Value>>,  // Per-field highlights
    pub highlight: Option<String>,                // Legacy single highlight
}
```

---

## 5. IndexManager Core Operations (index/manager.rs)

`IndexManager` is the central component managing multiple indexes:

**Design Philosophy:** Simple synchronous architecture suitable for CLI tools:
- `HashMap<String, IndexState>` for in-memory index registry
- File locking via `IndexLock` for process-level safety
- Synchronous operations (no async/locking complexity)

**Key Methods:**
```rust
impl IndexManager {
    pub fn new(base_path: impl AsRef<Path>) -> Self;
    pub fn load_indexes(&mut self) -> Result<()>;

    // Index lifecycle
    pub fn create_index(&mut self, name: &str, fields: Vec<FieldDef>, config: Option<IndexConfig>) -> Result<IndexSchema>;
    pub fn drop_index(&mut self, name: &str) -> Result<()>;
    pub fn list_indexes(&self) -> Vec<String>;
    pub fn get_stats(&self, name: &str) -> Result<IndexStats>;

    // Document operations
    pub fn add_document(&mut self, index_name: &str, document: Document) -> Result<()>;
    pub fn delete_document(&mut self, index_name: &str, doc_id: &str) -> Result<()>;
    pub fn commit(&mut self, index_name: &str) -> Result<()>;
    pub fn add_documents_batch(&mut self, index_name: &str, documents: Vec<BatchDocumentInput>, default_ttl: Option<String>, parallel: usize) -> Result<BatchResult>;

    // Search
    pub fn search(&self, index_name: &str, query: &SearchQuery) -> Result<Vec<SearchResult>>;

    // Maintenance
    pub fn compact(&mut self, index_name: &str) -> Result<()>;
}
```

---

## 6. File Locking (index/lock.rs)

Process-level exclusive locking to prevent concurrent CLI access:

```rust
pub struct IndexLock { ... }

impl IndexLock {
    pub fn acquire(index_path: &Path) -> Result<Self>;  // Blocking exclusive lock
}
// Automatically released on drop (RAII pattern)
// Lock file: <index_path>/.index.lock
```

---

## 7. Maintenance Operations (maintenance/)

| Operation | File | Description |
|-----------|------|-------------|
| Rebuild | `rebuild.rs` | Streaming pagination to avoid OOM, supports schema migration |
| Backup | `backup.rs` | `backup_index()`, `restore_index()` |
| Compaction | `compact.rs` | `compact_index()` merges segments and removes deleted docs |
| Expiration | `expire.rs` | `expire_documents()` removes documents past their TTL |

---

## 8. Configuration & Usage

**Default Index Directories:**
```
~/.gasket/tantivy/           # CLI indexes
```

**Usage Examples:**
```bash
# Create an index
cargo run -- index create --name myIndex \
  --fields '[{"name": "title", "type": "text"}, {"name": "content", "type": "text"}]'

# Add documents
cargo run -- doc add --index myIndex --id "doc1" \
  --fields '{"title": "Hello", "content": "World"}'

# Search
cargo run -- search --index myIndex \
  --query '{"text": "hello", "limit": 10}'

# Rebuild with schema migration
cargo run -- index rebuild --name myIndex \
  --fields '[{"name": "title", "type": "text"}, {"name": "body", "type": "text"}]'
```

---

## 9. File Index

| Feature | File Path |
|---------|----------|
| CLI entry | `tantivy/src/main.rs` |
| Library public API | `tantivy/src/lib.rs` |
| Error types | `tantivy/src/error.rs` |
| Index manager | `tantivy/src/index/manager.rs` |
| Schema definition | `tantivy/src/index/schema.rs` |
| Document types | `tantivy/src/index/document.rs` |
| Search types | `tantivy/src/index/search.rs` |
| File locking | `tantivy/src/index/lock.rs` |
| Maintenance operations | `tantivy/src/maintenance/*.rs` |
