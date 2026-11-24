// crates/audiolib-db/src/media_scan.rs
//! 图片素材扫描。
//!
//! 和音频扫描器刻意分开：音频那边有 vendor 表、指纹、并行元数据 worker 和一整套
//! 可恢复的扫描会话，图片不需要这些。这里只做三件事——按扩展名挑出图片、从文件头
//! 读出宽高、把结果写进 `media_assets`。
//!
//! 宽高不解码像素，只解析 PNG / JPEG / GIF / WebP 的头部。扫几万张图时，解码是
//! 秒级到分钟级的差别，而头部解析只读几百字节。
use super::media::{self, MediaCandidate, MediaKind, MediaUpsert, ThumbState};
use super::*;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

/// 当前支持的图片格式。和 `app/audiolib-desktop/Cargo.toml` 里 image 的 feature 对应，
/// 加格式要两边一起加，否则缩略图会解码失败。
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

/// 只读文件开头这么多字节找尺寸。JPEG 的 SOF 段几乎总在 EXIF 之后的前几十 KB 内。
const MAX_HEADER_BYTES: usize = 256 * 1024;

/// 进度回调的节流：每处理这么多文件报一次，避免 UI 被高频事件刷爆。
const PROGRESS_INTERVAL: u64 = 128;

#[derive(Clone, Copy, Debug, Default)]
pub struct MediaScanReport {
    /// 认出来并尝试入库的图片数。
    pub visited: u64,
    pub inserted: u64,
    pub updated: u64,
    pub unchanged: u64,
    /// 扩展名对但读不出尺寸的文件，仍然入库，宽高记 0。
    pub unreadable: u64,
    /// 上次在库、这次目录里没找到的记录。
    pub missing: u64,
}

pub struct MediaScanProgress {
    pub visited: u64,
    pub current: String,
}

/// 判断扩展名是否属于当前支持的图片格式。
pub fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .is_some_and(|value| IMAGE_EXTENSIONS.contains(&value.as_str()))
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default()
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 2)?;
    Some(u32::from(u16::from_be_bytes([slice[0], slice[1]])))
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 2)?;
    Some(u32::from(u16::from_le_bytes([slice[0], slice[1]])))
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le_u24(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 3)?;
    Some(u32::from(slice[0]) | (u32::from(slice[1]) << 8) | (u32::from(slice[2]) << 16))
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // 8 字节签名 + 4 字节长度 + "IHDR"，宽高紧跟其后。
    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be_u32(bytes, 16)?, be_u32(bytes, 20)?))
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"GIF87a") && !bytes.starts_with(b"GIF89a") {
        return None;
    }
    Some((le_u16(bytes, 6)?, le_u16(bytes, 8)?))
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" {
        return None;
    }
    match bytes.get(12..16)? {
        // 有扩展头的无损/动图，画布尺寸直接写在 VP8X 里。
        b"VP8X" => Some((le_u24(bytes, 24)? + 1, le_u24(bytes, 27)? + 1)),
        // 无损：14 位宽 + 14 位高，各减一。
        b"VP8L" => {
            if *bytes.get(20)? != 0x2f {
                return None;
            }
            let packed = u32::from_le_bytes([
                *bytes.get(21)?,
                *bytes.get(22)?,
                *bytes.get(23)?,
                *bytes.get(24)?,
            ]);
            Some(((packed & 0x3fff) + 1, ((packed >> 14) & 0x3fff) + 1))
        }
        // 有损：帧头 3 字节 + 起始码 9D 01 2A，之后是 14 位宽高。
        b"VP8 " => {
            if bytes.get(23..26)? != b"\x9d\x01\x2a" {
                return None;
            }
            Some((le_u16(bytes, 26)? & 0x3fff, le_u16(bytes, 28)? & 0x3fff))
        }
        _ => None,
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(0..2)? != b"\xff\xd8" {
        return None;
    }
    let mut index = 2usize;
    while index + 4 <= bytes.len() {
        if bytes[index] != 0xff {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];
        // 填充字节和独立标记没有长度字段。
        if marker == 0xff {
            index += 1;
            continue;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            index += 2;
            continue;
        }
        // 图像数据开始或文件结束，后面不会再有 SOF 了。
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        let length = be_u16(bytes, index + 2)? as usize;
        if length < 2 {
            return None;
        }
        // SOF0..SOF15，排除 DHT(C4)、JPG(C8)、DAC(CC)。
        let is_sof = (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_sof {
            let height = be_u16(bytes, index + 5)?;
            let width = be_u16(bytes, index + 7)?;
            return Some((width, height));
        }
        index += 2 + length;
    }
    None
}

/// 从文件头读出像素尺寸。读不出来返回 `None`，调用方会照常入库但宽高记 0。
pub fn probe_dimensions(path: &Path) -> Option<(u32, u32)> {
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = Vec::with_capacity(64 * 1024);
    (&mut file)
        .take(MAX_HEADER_BYTES as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    png_dimensions(&buffer)
        .or_else(|| jpeg_dimensions(&buffer))
        .or_else(|| gif_dimensions(&buffer))
        .or_else(|| webp_dimensions(&buffer))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

fn modified_nanos(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| i64::try_from(value.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 深度优先收集目录。跳过隐藏目录和 macOS 打包残留，避免把 `._xxx.png` 当成素材。
fn collect_directories(root: &Path, cancel: &AtomicBool) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        found.push(directory.clone());
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // 不跟随符号链接：素材盘里常有指向整块硬盘的链接，跟着走会绕圈。
            if !kind.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "__MACOSX" {
                continue;
            }
            pending.push(entry.path());
        }
    }
    found.sort();
    found
}

impl SqliteProvider {
    /// 扫描一个目录下的图片素材。
    ///
    /// 已有的记录按大小和修改时间判断是否需要更新；这次没扫到的旧记录标记为
    /// `missing`，不会删除，避免移动硬盘没挂载时整个库被清空。
    pub fn media_scan(
        &self,
        root: &Path,
        cancel: &AtomicBool,
        mut progress: impl FnMut(MediaScanProgress),
    ) -> Result<MediaScanReport> {
        if self.read_only {
            return Err(Error::Validation("只读库不能扫描素材".into()));
        }
        if !root.is_dir() {
            return Err(Error::Validation(format!(
                "素材目录不存在：{}",
                root.display()
            )));
        }

        let mut report = MediaScanReport::default();
        let writer = self.writer_unlocked()?;
        let root_text = root.to_string_lossy().to_string();

        for directory in collect_directories(root, cancel) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let directory_text = directory.to_string_lossy().to_string();
            let name = directory
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| directory_text.clone());
            let folder_id = media::ensure_folder(&writer, &directory_text, &name)?;

            // 这一层目录已入库的记录，扫到谁就从集合里删掉，剩下的就是这次没找到的。
            let mut stale: HashSet<String> = media::fingerprints(&writer, Some(folder_id))?
                .into_iter()
                .map(|(key, _, _)| key)
                .collect();

            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .filter(|entry| {
                    entry
                        .file_type()
                        .map(|kind| kind.is_file())
                        .unwrap_or(false)
                })
                .map(|entry| entry.path())
                .filter(|path| is_supported_image(path))
                .collect();
            files.sort();

            for path in files {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                let key = path_key(&path.to_string_lossy());
                stale.remove(&key);

                let dimensions = probe_dimensions(&path);
                if dimensions.is_none() {
                    report.unreadable += 1;
                }
                let (width, height) = dimensions.unwrap_or((0, 0));
                let filename = path
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_default();
                // 这两份字符串必须先落到变量上：直接写 `&path.to_string_lossy()`
                // 借的是临时值，出语句就失效了。
                let path_text = path.to_string_lossy().to_string();
                let format_text = extension_of(&path);
                let candidate = MediaCandidate {
                    file_path: &path_text,
                    filename: &filename,
                    folder_id: Some(folder_id),
                    kind: MediaKind::Image,
                    format: &format_text,
                    width,
                    height,
                    file_size: metadata.len(),
                    modified_at: modified_nanos(&metadata),
                };
                match media::upsert_asset(&writer, &candidate)? {
                    MediaUpsert::Inserted(_) => report.inserted += 1,
                    MediaUpsert::Updated(_) => report.updated += 1,
                    MediaUpsert::Unchanged(_) => report.unchanged += 1,
                }
                report.visited += 1;
                if report.visited % PROGRESS_INTERVAL == 0 {
                    progress(MediaScanProgress {
                        visited: report.visited,
                        current: filename,
                    });
                }
            }

            // 目录里彻底消失的文件才标丢失；权限不足读不到目录时不标，
            // 否则一次临时掉盘就会让整库变灰。
            for key in stale {
                let changed = writer
                    .execute(
                        "UPDATE media_assets SET missing=1 WHERE path_key=?1 AND folder_id=?2",
                        params![key, folder_id],
                    )
                    .map_err(db_error)?;
                report.missing += changed as u64;
            }
        }

        progress(MediaScanProgress {
            visited: report.visited,
            current: root_text,
        });
        Ok(report)
    }

    /// 按条件列出素材。查询条件和音频侧一样支持目录、关键词、标签、尺寸。
    pub fn media_list(&self, query: &media::MediaQuery) -> Result<Vec<media::MediaAsset>> {
        let reader = self.reader()?;
        media::list_assets(&reader, query)
    }

    pub fn media_count(&self, query: &media::MediaQuery) -> Result<u64> {
        let reader = self.reader()?;
        media::count_assets(&reader, query)
    }

    pub fn media_stats(&self) -> Result<media::MediaStats> {
        let reader = self.reader()?;
        media::stats(&reader)
    }

    /// 取一批待生成缩略图的记录，交给 desktop 侧的解码线程。
    pub fn media_pending_thumbnails(&self, limit: u32) -> Result<Vec<(i64, String)>> {
        let reader = self.reader()?;
        media::pending_thumbnails(&reader, limit)
    }

    /// 缩略图生成成功/失败后回写状态，避免每次启动都重试同一个坏文件。
    pub fn media_set_thumb_state(&self, id: i64, state: ThumbState) -> Result<()> {
        if self.read_only {
            return Err(Error::Validation("只读库不能写入缩略图状态".into()));
        }
        let writer = self.writer_unlocked()?;
        media::set_thumb_state(&writer, id, state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("audiolib-media-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    #[test]
    fn reads_png_dimensions() {
        let bytes = png_header(1920, 1080);
        assert_eq!(png_dimensions(&bytes), Some((1920, 1080)));
    }

    #[test]
    fn reads_gif_dimensions() {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&64u16.to_le_bytes());
        bytes.extend_from_slice(&32u16.to_le_bytes());
        assert_eq!(gif_dimensions(&bytes), Some((64, 32)));
    }

    #[test]
    fn reads_webp_lossless_dimensions() {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(b"VP8L");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0x2f);
        let packed: u32 = (99 & 0x3fff) | ((199 & 0x3fff) << 14);
        bytes.extend_from_slice(&packed.to_le_bytes());
        assert_eq!(webp_dimensions(&bytes), Some((100, 200)));
    }

    #[test]
    fn reads_jpeg_dimensions() {
        let mut bytes = vec![0xff, 0xd8];
        // APP0 段，长度 16，内容随便填。
        bytes.extend_from_slice(&[0xff, 0xe0, 0x00, 0x10]);
        bytes.extend_from_slice(&[0u8; 14]);
        // SOF0：长度 17，精度 8，高 480，宽 640。
        bytes.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&480u16.to_be_bytes());
        bytes.extend_from_slice(&640u16.to_be_bytes());
        bytes.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        assert_eq!(jpeg_dimensions(&bytes), Some((640, 480)));
    }

    #[test]
    fn extension_filter_ignores_audio_and_case() {
        assert!(is_supported_image(Path::new("/a/b.PNG")));
        assert!(is_supported_image(Path::new("/a/b.webp")));
        assert!(!is_supported_image(Path::new("/a/b.wav")));
        assert!(!is_supported_image(Path::new("/a/b")));
    }

    #[test]
    fn probe_reads_dimensions_from_disk() {
        let dir = temp_dir("probe");
        let file = dir.join("one.png");
        fs::write(&file, png_header(300, 200)).unwrap();
        assert_eq!(probe_dimensions(&file), Some((300, 200)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_skips_hidden_directories() {
        let dir = temp_dir("walk");
        fs::create_dir_all(dir.join("visible")).unwrap();
        fs::create_dir_all(dir.join(".hidden")).unwrap();
        fs::create_dir_all(dir.join("__MACOSX")).unwrap();
        let found = collect_directories(&dir, &AtomicBool::new(false));
        assert_eq!(found.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
