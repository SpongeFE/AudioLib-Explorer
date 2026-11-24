// crates/audiolib-db/src/media.rs
//! 素材库扩展：图片等非音频素材。
//!
//! 这条线与音频线并行，不复用 `audio_files`：音频表带着 sample_rate / bit_depth /
//! channels / duration 这些只对声音有意义的列，硬塞图片会让两边的查询都变脏。
//! 这里只新增一张 `media_assets`，目录树仍然共用 `folders` / `folder_closure`，
//! 所以侧栏的文件夹层级、计数和面包屑不需要重写。
//!
//! 建表不参与 `SCHEMA_VERSION` 版本体系，而是在每次打开写库时幂等补建。原因是
//! 音频的 schema 升级链（v5 → v8 → migration9）带着快照兼容、远程协议和可恢复
//! 迁移的约束，为了多一张独立表去动版本号，风险远大于收益。SQLite 允许多出
//! 不受管的表，`current_format_ready` 只校验必需对象是否齐全，不会因此报错。
//!
//! 关键词检索走 LIKE 而不是 FTS5。音频的 FTS 依赖 jieba 分词和自定义折叠函数，
//! 图片库规模通常在几千到几万行，`LIKE '%词%'` 在这个量级是毫秒级，不值得为它
//! 再挂一套 tokenizer 和触发器。
use super::*;

/// 素材大类。目前只落地图片，其余先占位，避免以后加类型时改表结构。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Image = 1,
    Video = 2,
    Lut = 3,
    Template = 4,
    Font = 5,
}

impl MediaKind {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    /// 未知值按图片处理：宁可显示成图片，也不要让一条记录因为类型号不认识而消失。
    pub fn from_i64(value: i64) -> Self {
        match value {
            2 => Self::Video,
            3 => Self::Lut,
            4 => Self::Template,
            5 => Self::Font,
            _ => Self::Image,
        }
    }
}

/// 缩略图状态。`Missing` 与「未生成」分开记，否则每次扫描都会重试解码坏文件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbState {
    Pending = 0,
    Ready = 1,
    Failed = 2,
}

impl ThumbState {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            1 => Self::Ready,
            2 => Self::Failed,
            _ => Self::Pending,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MediaSort {
    #[default]
    NameAsc,
    NameDesc,
    NewestFirst,
    OldestFirst,
    LargestFirst,
    SmallestFirst,
}

impl MediaSort {
    /// 只返回常量片段，排序方向从不来自调用方的字符串。
    fn order_by(self) -> &'static str {
        match self {
            Self::NameAsc => "m.filename COLLATE NOCASE ASC, m.id ASC",
            Self::NameDesc => "m.filename COLLATE NOCASE DESC, m.id DESC",
            Self::NewestFirst => "m.modified_at DESC, m.id DESC",
            Self::OldestFirst => "m.modified_at ASC, m.id ASC",
            Self::LargestFirst => "m.file_size DESC, m.id DESC",
            Self::SmallestFirst => "m.file_size ASC, m.id ASC",
        }
    }
}

/// 一条素材记录。字段刻意保持扁平，方便直接映射到 UI 行结构。
#[derive(Clone, Debug, Default)]
pub struct MediaAsset {
    pub id: i64,
    pub file_path: String,
    pub filename: String,
    pub folder_id: Option<i64>,
    pub kind: i64,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub tags: String,
    pub color: Option<String>,
    pub notes: String,
    pub favorite: bool,
    pub rating: u8,
    pub thumb_state: i64,
    pub modified_at: i64,
    pub missing: bool,
}

/// 扫描器交给数据层的一条候选记录。尺寸等元数据在 desktop 侧用 image crate 读好再传进来，
/// 这样 audiolib-db 不需要依赖图片解码库。
pub struct MediaCandidate<'a> {
    pub file_path: &'a str,
    pub filename: &'a str,
    pub folder_id: Option<i64>,
    pub kind: MediaKind,
    pub format: &'a str,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub modified_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaUpsert {
    Inserted(i64),
    Updated(i64),
    Unchanged(i64),
}

impl MediaUpsert {
    pub fn id(self) -> i64 {
        match self {
            Self::Inserted(id) | Self::Updated(id) | Self::Unchanged(id) => id,
        }
    }
}

/// 列表查询条件。所有字段都是可选的，默认取全部未丢失的图片。
#[derive(Clone, Debug, Default)]
pub struct MediaQuery {
    /// 只查这一层目录（`folders.id`）。
    pub folder_id: Option<i64>,
    /// 查这棵子树，含自身。按 `folders.folder_path` 前缀展开，不依赖 folder_closure，
    /// 图片目录因此不需要参与音频的目录归并流程。
    pub folder_prefix: Option<String>,
    pub kind: Option<MediaKind>,
    pub keyword: Option<String>,
    pub tag: Option<String>,
    pub format: Option<String>,
    pub favorites_only: bool,
    pub include_missing: bool,
    pub min_width: Option<u32>,
    pub max_width: Option<u32>,
    pub sort: MediaSort,
    pub limit: u32,
    pub offset: u32,
}

impl MediaQuery {
    /// 拼出 WHERE 子句和参数。值一律走占位符，只有排序片段来自常量。
    fn build(&self) -> (String, Vec<Value>) {
        let mut clauses: Vec<&str> = Vec::new();
        let mut values: Vec<Value> = Vec::new();

        if !self.include_missing {
            clauses.push("m.missing=0");
        }
        if let Some(folder) = self.folder_id {
            clauses.push("m.folder_id=?");
            values.push(Value::Integer(folder));
        }
        if let Some(prefix) = self
            .folder_prefix
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            clauses.push(
                "m.folder_id IN (SELECT id FROM folders \
                 WHERE folder_path=? OR folder_path LIKE ? ESCAPE '\\')",
            );
            values.push(Value::Text(prefix.clone()));
            values.push(Value::Text(format!(
                "{}/%",
                escape_like(prefix.trim_end_matches('/'))
            )));
        }
        if let Some(kind) = self.kind {
            clauses.push("m.kind=?");
            values.push(Value::Integer(kind.as_i64()));
        }
        if let Some(format) = self.format.as_ref().filter(|value| !value.is_empty()) {
            clauses.push("m.format=?");
            values.push(Value::Text(format.to_lowercase()));
        }
        if self.favorites_only {
            clauses.push("m.favorite=1");
        }
        if let Some(min) = self.min_width {
            clauses.push("m.width>=?");
            values.push(Value::Integer(i64::from(min)));
        }
        if let Some(max) = self.max_width {
            clauses.push("m.width<=?");
            values.push(Value::Integer(i64::from(max)));
        }
        if let Some(keyword) = self
            .keyword
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            // 文件名、标签、备注三处任意命中即可。中文子串用 LIKE 直接匹配，
            // 不需要分词，因为这里找的是「包含这几个字」而不是语义相关。
            clauses.push("(m.filename LIKE ? ESCAPE '\\' OR m.user_tags LIKE ? ESCAPE '\\' OR m.notes LIKE ? ESCAPE '\\')");
            let pattern = format!("%{}%", escape_like(keyword));
            values.push(Value::Text(pattern.clone()));
            values.push(Value::Text(pattern.clone()));
            values.push(Value::Text(pattern));
        }
        if let Some(tag) = self
            .tag
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            clauses.push("m.user_tags LIKE ? ESCAPE '\\'");
            values.push(Value::Text(format!("%{}%", escape_like(tag))));
        }

        let where_sql = if clauses.is_empty() {
            String::from("1=1")
        } else {
            clauses.join(" AND ")
        };
        (where_sql, values)
    }
}

/// `LIKE` 里的 `%` `_` `\` 必须转义，否则用户搜「100%」会变成通配。
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            other => escaped.push(other),
        }
    }
    escaped
}

const MEDIA_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS media_assets (
  id INTEGER PRIMARY KEY,
  file_path TEXT NOT NULL UNIQUE,
  path_key TEXT NOT NULL,
  filename TEXT NOT NULL,
  folder_id INTEGER,
  kind INTEGER NOT NULL DEFAULT 1,
  format TEXT NOT NULL DEFAULT '',
  width INTEGER NOT NULL DEFAULT 0,
  height INTEGER NOT NULL DEFAULT 0,
  file_size INTEGER NOT NULL DEFAULT 0,
  user_tags TEXT NOT NULL DEFAULT '',
  user_color TEXT,
  notes TEXT NOT NULL DEFAULT '',
  favorite INTEGER NOT NULL DEFAULT 0,
  rating INTEGER NOT NULL DEFAULT 0,
  palette TEXT,
  thumb_state INTEGER NOT NULL DEFAULT 0,
  modified_at INTEGER NOT NULL DEFAULT 0,
  missing INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_media_folder ON media_assets(folder_id, filename COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_media_kind ON media_assets(kind, filename COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_media_path_key ON media_assets(path_key);
CREATE INDEX IF NOT EXISTS idx_media_modified ON media_assets(modified_at DESC);
CREATE INDEX IF NOT EXISTS idx_media_size ON media_assets(file_size DESC);
CREATE INDEX IF NOT EXISTS idx_media_missing ON media_assets(missing) WHERE missing=1;
CREATE INDEX IF NOT EXISTS idx_media_favorite ON media_assets(favorite) WHERE favorite=1;
";

/// 幂等补建素材表。每次打开写库调用一次，老库因此自动获得这张表。
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(MEDIA_SCHEMA).map_err(db_error)
}

fn projection() -> &'static str {
    "m.id, m.file_path, m.filename, m.folder_id, m.kind, m.format, m.width, m.height, \
     m.file_size, m.user_tags, m.user_color, m.notes, m.favorite, m.rating, m.thumb_state, \
     m.modified_at, m.missing"
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<MediaAsset> {
    let width: i64 = row.get(6)?;
    let height: i64 = row.get(7)?;
    let size: i64 = row.get(8)?;
    let rating: i64 = row.get(13)?;
    Ok(MediaAsset {
        id: row.get(0)?,
        file_path: row.get(1)?,
        filename: row.get(2)?,
        folder_id: row.get(3)?,
        kind: row.get(4)?,
        format: row.get(5)?,
        width: u32::try_from(width).unwrap_or(0),
        height: u32::try_from(height).unwrap_or(0),
        file_size: u64::try_from(size).unwrap_or(0),
        tags: row.get(9)?,
        color: row.get(10)?,
        notes: row.get(11)?,
        favorite: row.get::<_, i64>(12)? != 0,
        rating: u8::try_from(rating).unwrap_or(0),
        thumb_state: row.get(14)?,
        modified_at: row.get(15)?,
        missing: row.get::<_, i64>(16)? != 0,
    })
}

/// 写入或刷新一条素材。大小与修改时间都没变时不做无谓的 UPDATE，
/// 这样重复扫描一个大图片目录不会产生大量 WAL 写入。
pub fn upsert_asset(conn: &Connection, candidate: &MediaCandidate<'_>) -> Result<MediaUpsert> {
    let key = path_key(candidate.file_path);
    let existing: Option<(i64, i64, i64, i64)> = conn
        .query_row(
            "SELECT id, file_size, modified_at, thumb_state FROM media_assets WHERE path_key=?1 LIMIT 1",
            [&key],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(db_error)?;

    let size = i64::try_from(candidate.file_size).unwrap_or(i64::MAX);
    let width = i64::from(candidate.width);
    let height = i64::from(candidate.height);

    if let Some((id, prior_size, prior_modified, thumb_state)) = existing {
        let unchanged = prior_size == size && prior_modified == candidate.modified_at;
        if unchanged && thumb_state != i64::from(ThumbState::Pending.as_i64()) {
            return Ok(MediaUpsert::Unchanged(id));
        }
        conn.execute(
            "UPDATE media_assets SET file_path=?2, filename=?3, folder_id=?4, kind=?5, format=?6, \
             width=?7, height=?8, file_size=?9, modified_at=?10, missing=0, \
             thumb_state=CASE WHEN ?11=0 THEN thumb_state ELSE 0 END \
             WHERE id=?1",
            params![
                id,
                candidate.file_path,
                candidate.filename,
                candidate.folder_id,
                candidate.kind.as_i64(),
                candidate.format.to_lowercase(),
                width,
                height,
                size,
                candidate.modified_at,
                if unchanged { 1 } else { 0 },
            ],
        )
        .map_err(db_error)?;
        return Ok(MediaUpsert::Updated(id));
    }

    conn.execute(
        "INSERT INTO media_assets \
         (file_path, path_key, filename, folder_id, kind, format, width, height, file_size, modified_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            candidate.file_path,
            key,
            candidate.filename,
            candidate.folder_id,
            candidate.kind.as_i64(),
            candidate.format.to_lowercase(),
            width,
            height,
            size,
            candidate.modified_at,
        ],
    )
    .map_err(db_error)?;
    Ok(MediaUpsert::Inserted(conn.last_insert_rowid()))
}

pub fn list_assets(conn: &Connection, query: &MediaQuery) -> Result<Vec<MediaAsset>> {
    let (where_sql, mut values) = query.build();
    let limit = if query.limit == 0 { 500 } else { query.limit };
    values.push(Value::Integer(i64::from(limit)));
    values.push(Value::Integer(i64::from(query.offset)));
    let sql = format!(
        "SELECT {} FROM media_assets m WHERE {} ORDER BY {} LIMIT ? OFFSET ?",
        projection(),
        where_sql,
        query.sort.order_by()
    );
    let mut statement = conn.prepare(&sql).map_err(db_error)?;
    let rows = statement
        .query_map(params_from_iter(values), read_row)
        .map_err(db_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
}

pub fn count_assets(conn: &Connection, query: &MediaQuery) -> Result<u64> {
    let (where_sql, values) = query.build();
    let sql = format!("SELECT COUNT(*) FROM media_assets m WHERE {where_sql}");
    let count: i64 = conn
        .query_row(&sql, params_from_iter(values), |row| row.get(0))
        .map_err(db_error)?;
    Ok(u64::try_from(count).unwrap_or(0))
}

pub fn get_asset(conn: &Connection, id: i64) -> Result<Option<MediaAsset>> {
    let sql = format!("SELECT {} FROM media_assets m WHERE m.id=?1", projection());
    conn.query_row(&sql, [id], read_row)
        .optional()
        .map_err(db_error)
}

/// 按目录路径找 `folders.id`。图片扫描时复用音频已经建好的目录树。
pub fn folder_id_for(conn: &Connection, directory: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM folders WHERE folder_path=?1 LIMIT 1",
        [directory],
        |row| row.get(0),
    )
    .optional()
    .map_err(db_error)
}

/// 图片目录不在音频目录树里时补一条。`folders` 的其余列都有默认值，
/// 计数由音频扫描器统一维护，这里只保证路径可查。
pub fn ensure_folder(conn: &Connection, directory: &str, name: &str) -> Result<i64> {
    if let Some(id) = folder_id_for(conn, directory)? {
        return Ok(id);
    }
    conn.execute(
        "INSERT OR IGNORE INTO folders (folder_path, folder_name) VALUES (?1, ?2)",
        params![directory, name],
    )
    .map_err(db_error)?;
    folder_id_for(conn, directory)?
        .ok_or_else(|| Error::Validation(format!("素材目录写入失败：{directory}")))
}

pub fn set_tags(conn: &Connection, id: i64, tags: &str) -> Result<()> {
    conn.execute(
        "UPDATE media_assets SET user_tags=?2 WHERE id=?1",
        params![id, tags],
    )
    .map_err(db_error)?;
    Ok(())
}

pub fn set_favorite(conn: &Connection, id: i64, favorite: bool) -> Result<()> {
    conn.execute(
        "UPDATE media_assets SET favorite=?2 WHERE id=?1",
        params![id, if favorite { 1 } else { 0 }],
    )
    .map_err(db_error)?;
    Ok(())
}

pub fn set_notes(conn: &Connection, id: i64, notes: &str) -> Result<()> {
    conn.execute(
        "UPDATE media_assets SET notes=?2 WHERE id=?1",
        params![id, notes],
    )
    .map_err(db_error)?;
    Ok(())
}

pub fn set_rating(conn: &Connection, id: i64, rating: u8) -> Result<()> {
    conn.execute(
        "UPDATE media_assets SET rating=?2 WHERE id=?1",
        params![id, i64::from(rating.min(5))],
    )
    .map_err(db_error)?;
    Ok(())
}

pub fn set_thumb_state(conn: &Connection, id: i64, state: ThumbState) -> Result<()> {
    conn.execute(
        "UPDATE media_assets SET thumb_state=?2 WHERE id=?1",
        params![id, state.as_i64()],
    )
    .map_err(db_error)?;
    Ok(())
}

pub fn set_palette(conn: &Connection, id: i64, palette: &str) -> Result<()> {
    conn.execute(
        "UPDATE media_assets SET palette=?2 WHERE id=?1",
        params![id, palette],
    )
    .map_err(db_error)?;
    Ok(())
}

pub fn set_missing(conn: &Connection, id: i64, missing: bool) -> Result<()> {
    conn.execute(
        "UPDATE media_assets SET missing=?2 WHERE id=?1",
        params![id, if missing { 1 } else { 0 }],
    )
    .map_err(db_error)?;
    Ok(())
}

/// 清空某棵子树下所有素材的「丢失」标记，供重新扫描前调用。
pub fn clear_missing(conn: &Connection, folder_prefix: Option<&str>) -> Result<u64> {
    let changed = match folder_prefix.filter(|value| !value.is_empty()) {
        Some(prefix) => conn
            .execute(
                "UPDATE media_assets SET missing=0 WHERE folder_id IN \
                 (SELECT id FROM folders WHERE folder_path=?1 OR folder_path LIKE ?2 ESCAPE '\\')",
                params![
                    prefix,
                    format!("{}/%", escape_like(prefix.trim_end_matches('/')))
                ],
            )
            .map_err(db_error)?,
        None => conn
            .execute("UPDATE media_assets SET missing=0 WHERE missing=1", [])
            .map_err(db_error)?,
    };
    Ok(changed as u64)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MediaStats {
    pub total: u64,
    pub images: u64,
    pub favorites: u64,
    pub missing: u64,
    pub thumbs_ready: u64,
    pub total_bytes: u64,
}

pub fn stats(conn: &Connection) -> Result<MediaStats> {
    let mut statement = conn
        .prepare(
            "SELECT COUNT(*), \
             COALESCE(SUM(CASE WHEN kind=1 THEN 1 ELSE 0 END),0), \
             COALESCE(SUM(favorite),0), \
             COALESCE(SUM(missing),0), \
             COALESCE(SUM(CASE WHEN thumb_state=1 THEN 1 ELSE 0 END),0), \
             COALESCE(SUM(file_size),0) \
             FROM media_assets",
        )
        .map_err(db_error)?;
    statement
        .query_row([], |row| {
            let total: i64 = row.get(0)?;
            let images: i64 = row.get(1)?;
            let favorites: i64 = row.get(2)?;
            let missing: i64 = row.get(3)?;
            let ready: i64 = row.get(4)?;
            let bytes: i64 = row.get(5)?;
            Ok(MediaStats {
                total: u64::try_from(total).unwrap_or(0),
                images: u64::try_from(images).unwrap_or(0),
                favorites: u64::try_from(favorites).unwrap_or(0),
                missing: u64::try_from(missing).unwrap_or(0),
                thumbs_ready: u64::try_from(ready).unwrap_or(0),
                total_bytes: u64::try_from(bytes).unwrap_or(0),
            })
        })
        .map_err(db_error)
}

/// 目录下已入库素材的 (path_key, file_size, modified_at)，用于扫描时跳过没变的文件。
pub fn fingerprints(conn: &Connection, folder_id: Option<i64>) -> Result<Vec<(String, i64, i64)>> {
    let sql = match folder_id {
        Some(_) => "SELECT path_key, file_size, modified_at FROM media_assets WHERE folder_id=?1",
        None => "SELECT path_key, file_size, modified_at FROM media_assets",
    };
    fn read(row: &Row<'_>) -> rusqlite::Result<(String, i64, i64)> {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }
    let mut statement = conn.prepare(sql).map_err(db_error)?;
    let rows = match folder_id {
        Some(id) => statement.query_map([id], read).map_err(db_error)?,
        None => statement.query_map([], read).map_err(db_error)?,
    };
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
}

/// 待生成缩略图的记录：文件路径 + 已经定好的缓存名。
pub fn pending_thumbnails(conn: &Connection, limit: u32) -> Result<Vec<(i64, String)>> {
    let mut statement = conn
        .prepare(
            "SELECT id, file_path FROM media_assets \
             WHERE thumb_state=0 AND missing=0 AND kind=1 ORDER BY id LIMIT ?1",
        )
        .map_err(db_error)?;
    let rows = statement
        .query_map([i64::from(if limit == 0 { 200 } else { limit })], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(db_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE folders (id INTEGER PRIMARY KEY, folder_path TEXT NOT NULL UNIQUE, \
             folder_name TEXT NOT NULL, parent_id INTEGER, depth INTEGER NOT NULL DEFAULT 0);\
             CREATE TABLE folder_closure (ancestor_id INTEGER NOT NULL, descendant_id INTEGER NOT NULL, \
             depth INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(ancestor_id,descendant_id));",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    fn candidate<'a>(path: &'a str, width: u32) -> MediaCandidate<'a> {
        MediaCandidate {
            file_path: path,
            filename: path.rsplit('/').next().unwrap_or(path),
            folder_id: None,
            kind: MediaKind::Image,
            format: "png",
            width,
            height: width / 2,
            file_size: 1024,
            modified_at: 1,
        }
    }

    #[test]
    fn schema_is_idempotent() {
        let conn = database();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    #[test]
    fn upsert_reports_insert_then_unchanged() {
        let conn = database();
        assert!(matches!(
            upsert_asset(&conn, &candidate("/a/one.png", 100)).unwrap(),
            MediaUpsert::Inserted(_)
        ));
        assert!(matches!(
            upsert_asset(&conn, &candidate("/a/one.png", 100)).unwrap(),
            MediaUpsert::Unchanged(_)
        ));
    }

    #[test]
    fn upsert_matches_path_case_insensitively() {
        let conn = database();
        upsert_asset(&conn, &candidate("/A/One.PNG", 100)).unwrap();
        assert!(matches!(
            upsert_asset(&conn, &candidate("/a/one.png", 100)).unwrap(),
            MediaUpsert::Unchanged(_)
        ));
        assert_eq!(stats(&conn).unwrap().total, 1);
    }

    #[test]
    fn query_filters_by_keyword_and_size() {
        let conn = database();
        upsert_asset(&conn, &candidate("/a/logo-dark.png", 200)).unwrap();
        upsert_asset(&conn, &candidate("/a/photo.jpg", 4000)).unwrap();

        let mut query = MediaQuery {
            keyword: Some("logo".into()),
            ..Default::default()
        };
        assert_eq!(list_assets(&conn, &query).unwrap().len(), 1);

        query.keyword = None;
        query.min_width = Some(1000);
        let wide = list_assets(&conn, &query).unwrap();
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0].filename, "photo.jpg");
    }

    #[test]
    fn like_escapes_wildcards() {
        let conn = database();
        upsert_asset(&conn, &candidate("/a/100%.png", 10)).unwrap();
        upsert_asset(&conn, &candidate("/a/1000.png", 10)).unwrap();
        let query = MediaQuery {
            keyword: Some("100%".into()),
            ..Default::default()
        };
        let found = list_assets(&conn, &query).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].filename, "100%.png");
    }

    #[test]
    fn missing_rows_are_hidden_by_default() {
        let conn = database();
        let id = upsert_asset(&conn, &candidate("/a/x.png", 10))
            .unwrap()
            .id();
        set_missing(&conn, id, true).unwrap();
        assert_eq!(list_assets(&conn, &MediaQuery::default()).unwrap().len(), 0);
        let query = MediaQuery {
            include_missing: true,
            ..Default::default()
        };
        assert_eq!(list_assets(&conn, &query).unwrap().len(), 1);
    }
}
