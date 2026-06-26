# Tantivy 模块设计文档

## 1. 概述

**`gasket-tantivy`** 是一个 CLI 工具，用于管理 Tantivy 全文搜索索引。

**核心职责：**
- 多独立索引的创建、管理、查询
- 支持 BM25 全文搜索、过滤、高亮
- 索引维护操作 (备份、压缩、重建、过期清理)
- 进程级文件锁防止并发访问
- 默认存储路径：`~/.gasket/tantivy/`

---

## 2. 目录结构

```
gasket/tantivy/
├── src/
│   ├── lib.rs                      # 库入口 (导出公共 API)
│   ├── main.rs                     # CLI 入口点
│   ├── error.rs                    # 错误类型
│   ├── index/
│   │   ├── mod.rs                  # 索引模块导出
│   │   ├── document.rs             # Document, BatchDocumentInput 类型
│   │   ├── schema.rs               # Schema 定义 (FieldDef, FieldType, IndexSchema)
│   │   ├── manager.rs             # IndexManager (核心索引操作)
│   │   ├── search.rs               # SearchQuery, SearchResult 类型
│   │   └── lock.rs                 # 文件锁 IndexLock
│   └── maintenance/
│       ├── mod.rs                  # 维护模块导出
│       ├── backup.rs               # 备份/恢复操作
│       ├── compact.rs              # 压缩操作
│       ├── expire.rs               # TTL/文档过期
│       ├── rebuild.rs              # 索引重建 (支持 schema 迁移)
│       └── stats.rs                # IndexHealth 状态
└── tests/
    └── integration_test.rs
```

---

## 3. CLI 命令

### 索引管理
```bash
index create --name <name> --fields <json-array> [--default-ttl <duration>]
index list
index stats [--name <name>]
index drop --name <name>
index compact --name <name>
index rebuild --name <name> [--fields <json-array>]
```

### 文档操作
```bash
doc add --index <name> --id <id> --fields <json-object> [--ttl <duration>]
doc add-batch --index <name> (--file <path> | --documents <json>) [--ttl <duration>] [--parallel <n>]
doc delete --index <name> --id <id>
doc commit --index <name>
```

### 搜索
```bash
search --index <name> --query <json-query>
```

**SearchQuery JSON 格式:**
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

## 4. 核心数据类型

### 4.1 Schema 类型 (index/schema.rs)

**FieldType** - 支持的字段类型：
- `Text` - 全文索引 (分词用于 BM25)
- `String` - 精确匹配 (不分词)
- `I64` / `F64` - 数值字段
- `DateTime` - ISO 8601 时间戳
- `StringArray` - 多值字符串 (标签)
- `Json` - 仅存储的 JSON

**FieldDef** - 字段定义：
```rust
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub indexed: bool,  // 包含在搜索索引
    pub stored: bool,   // 在搜索结果中返回
}
```

### 4.2 文档类型 (index/document.rs)

```rust
pub struct Document {
    pub id: String,
    pub fields: Map<String, Value>,
    pub expires_at: Option<DateTime<Utc>>,  // TTL 支持
}
```

### 4.3 搜索类型 (index/search.rs)

```rust
pub struct SearchQuery {
    pub text: Option<String>,           // 全文搜索
    pub filters: Vec<FieldFilter>,      // 字段过滤
    pub limit: usize,                   // 最大结果数 (默认: 10)
    pub offset: usize,                  // 分页偏移
    pub sort: Option<SortConfig>,       // 排序配置
    pub highlight: Option<HighlightConfig>,  // 高亮配置
}

pub struct SearchResult {
    pub id: String,
    pub fields: Map<String, Value>,
    pub score: f32,
    pub highlights: Option<Map<String, Value>>,  // 每字段高亮
    pub highlight: Option<String>,                // 遗留单高亮
}
```

---

## 5. IndexManager 核心操作 (index/manager.rs)

`IndexManager` 是管理多个索引的中心组件：

**设计哲学：** 简单同步架构，适合 CLI 工具：
- `HashMap<String, IndexState>` 内存索引注册表
- 文件锁 `IndexLock` 实现进程级安全
- 同步操作 (无需 async/锁复杂度)

**主要方法：**
```rust
impl IndexManager {
    pub fn new(base_path: impl AsRef<Path>) -> Self;
    pub fn load_indexes(&mut self) -> Result<()>;

    // 索引生命周期
    pub fn create_index(&mut self, name: &str, fields: Vec<FieldDef>, config: Option<IndexConfig>) -> Result<IndexSchema>;
    pub fn drop_index(&mut self, name: &str) -> Result<()>;
    pub fn list_indexes(&self) -> Vec<String>;
    pub fn get_stats(&self, name: &str) -> Result<IndexStats>;

    // 文档操作
    pub fn add_document(&mut self, index_name: &str, document: Document) -> Result<()>;
    pub fn delete_document(&mut self, index_name: &str, doc_id: &str) -> Result<()>;
    pub fn commit(&mut self, index_name: &str) -> Result<()>;
    pub fn add_documents_batch(&mut self, index_name: &str, documents: Vec<BatchDocumentInput>, default_ttl: Option<String>, parallel: usize) -> Result<BatchResult>;

    // 搜索
    pub fn search(&self, index_name: &str, query: &SearchQuery) -> Result<Vec<SearchResult>>;

    // 维护
    pub fn compact(&mut self, index_name: &str) -> Result<()>;
}
```

---

## 6. 文件锁 (index/lock.rs)

进程级独占锁，防止并发 CLI 访问：

```rust
pub struct IndexLock { ... }

impl IndexLock {
    pub fn acquire(index_path: &Path) -> Result<Self>;  // 阻塞式独占锁
}
// 自动在 drop 时释放 (RAII 模式)
// 锁文件: <index_path>/.index.lock
```

---

## 7. 维护操作 (maintenance/)

| 操作 | 文件 | 说明 |
|------|------|------|
| 重建 | `rebuild.rs` | 流式分页避免 OOM，支持 schema 迁移 |
| 备份 | `backup.rs` | `backup_index()`, `restore_index()` |
| 压缩 | `compact.rs` | `compact_index()` 合并段并移除已删除文档 |
| 过期 | `expire.rs` | `expire_documents()` 移除超过 TTL 的文档 |

---

## 8. 配置与使用

**默认索引目录：**
```
~/.gasket/tantivy/           # CLI 索引
```

**使用示例：**
```bash
# 创建索引
cargo run -- index create --name myIndex \
  --fields '[{"name": "title", "type": "text"}, {"name": "content", "type": "text"}]'

# 添加文档
cargo run -- doc add --index myIndex --id "doc1" \
  --fields '{"title": "Hello", "content": "World"}'

# 搜索
cargo run -- search --index myIndex \
  --query '{"text": "hello", "limit": 10}'

# 重建索引 (支持 schema 迁移)
cargo run -- index rebuild --name myIndex \
  --fields '[{"name": "title", "type": "text"}, {"name": "body", "type": "text"}]'
```

---

## 9. 文件索引

| 功能 | 文件路径 |
|------|----------|
| CLI 入口 | `tantivy/src/main.rs` |
| 库公共 API | `tantivy/src/lib.rs` |
| 错误类型 | `tantivy/src/error.rs` |
| 索引管理 | `tantivy/src/index/manager.rs` |
| Schema 定义 | `tantivy/src/index/schema.rs` |
| 文档类型 | `tantivy/src/index/document.rs` |
| 搜索类型 | `tantivy/src/index/search.rs` |
| 文件锁 | `tantivy/src/index/lock.rs` |
| 维护操作 | `tantivy/src/maintenance/*.rs` |
