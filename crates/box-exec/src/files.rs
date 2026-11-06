//! Files, as the user they belong to. Every route here does its work under that user's file
//! identity, so what a link leads to is decided by the kernel and not by a check made first: a
//! Bot's request reaches what that Bot's user can reach, and `box-exec`'s own files stay out of it.

use std::path::Path;

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use box_common::{resolve_in_canonical_jail, ApiError};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::users::on_disk;
use crate::AppState;

fn b64_encode(input: &[u8]) -> String {
    BASE64.encode(input)
}

fn b64_decode(input: &str) -> Result<Vec<u8>, &'static str> {
    if input.bytes().any(|b| b.is_ascii_whitespace()) {
        let mut filtered = Vec::with_capacity(input.len());
        filtered.extend(input.bytes().filter(|b| !b.is_ascii_whitespace()));
        BASE64.decode(filtered).map_err(|_| "invalid base64")
    } else {
        BASE64.decode(input).map_err(|_| "invalid base64")
    }
}

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    /// Whose folder this is: the file work runs as that Bot's user. The shared user without it.
    pub bot: Option<String>,
    pub path: Option<String>,
    pub encoding: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileGetResponse {
    File {
        path: String,
        size: u64,
        encoding: &'static str,
        content: String,
    },
    Directory {
        path: String,
        entries: Vec<DirEntry>,
        truncated: bool,
    },
}

#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: &'static str,
    pub size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct FilePutRequest {
    /// Whose folder this is: the file work runs as that Bot's user. The shared user without it.
    pub bot: Option<String>,
    pub path: String,
    pub content: String,
    pub encoding: Option<String>,
    pub create_dirs: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct FilePutResponse {
    pub path: String,
    pub bytes_written: u64,
}

#[derive(Debug, Deserialize)]
pub struct FileDeleteQuery {
    /// Whose folder this is: the file work runs as that Bot's user. The shared user without it.
    pub bot: Option<String>,
    pub path: Option<String>,
    pub recursive: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FileDeleteResponse {
    pub path: String,
    pub deleted: bool,
}

#[derive(Debug, Deserialize)]
pub struct MkdirRequest {
    /// Whose folder this is: the file work runs as that Bot's user. The shared user without it.
    pub bot: Option<String>,
    pub path: String,
    pub parents: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct MkdirResponse {
    pub path: String,
    pub created: bool,
}

#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    /// Whose folder this is: the file work runs as that Bot's user. The shared user without it.
    pub bot: Option<String>,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Serialize)]
pub struct RenameResponse {
    pub from: String,
    pub to: String,
}

pub async fn get(
    State(state): State<AppState>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileGetResponse>, ApiError> {
    let who = state.users.who(query.bot.as_deref())?;
    on_disk(who, move || get_now(&state, query)).await?
}

fn get_now(state: &AppState, query: FileQuery) -> Result<Json<FileGetResponse>, ApiError> {
    let user_path = query.path.as_deref().unwrap_or("");
    let resolved = resolve_in_canonical_jail(&state.workspace, user_path)?;
    let meta = std::fs::metadata(&resolved).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found(format!("not found: {}", resolved.display()))
        } else {
            ApiError::io(err.to_string())
        }
    })?;

    if meta.is_dir() {
        let mut entries = Vec::new();
        let mut truncated = false;
        let read = std::fs::read_dir(&resolved).map_err(|err| ApiError::io(err.to_string()))?;
        for entry in read {
            let entry = entry.map_err(|err| ApiError::io(err.to_string()))?;
            if entries.len() >= state.max_dir_entries {
                truncated = true;
                break;
            }
            let file_type = entry
                .file_type()
                .map_err(|err| ApiError::io(err.to_string()))?;
            let kind = if file_type.is_dir() {
                "directory"
            } else if file_type.is_symlink() {
                "symlink"
            } else {
                "file"
            };
            let size = if file_type.is_file() {
                entry.metadata().ok().map(|m| m.len())
            } else {
                None
            };
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
                size,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        return Ok(Json(FileGetResponse::Directory {
            path: display_under_workspace(&state.workspace, &resolved),
            entries,
            truncated,
        }));
    }

    if meta.len() > state.max_file_bytes {
        return Err(ApiError::payload_too_large(format!(
            "file is {} bytes; max is {}",
            meta.len(),
            state.max_file_bytes
        )));
    }

    let bytes = std::fs::read(&resolved).map_err(|err| ApiError::io(err.to_string()))?;
    let want = query.encoding.as_deref().unwrap_or("utf8");
    if want.eq_ignore_ascii_case("raw") {
        return Err(ApiError::invalid_request(
            "raw encoding is on GET /v1/files/raw (application/octet-stream)",
        ));
    }

    let (encoding, content) = if want.eq_ignore_ascii_case("base64") {
        ("base64", b64_encode(&bytes))
    } else if want.eq_ignore_ascii_case("utf8") || want.eq_ignore_ascii_case("text") {
        match String::from_utf8(bytes) {
            Ok(text) => ("utf8", text),
            Err(err) => ("base64", b64_encode(&err.into_bytes())),
        }
    } else {
        return Err(ApiError::invalid_request(format!(
            "unsupported encoding '{want}'"
        )));
    };

    Ok(Json(FileGetResponse::File {
        path: display_under_workspace(&state.workspace, &resolved),
        size: meta.len(),
        encoding,
        content,
    }))
}

#[derive(Debug, Deserialize)]
pub struct FileRawQuery {
    /// Whose folder this is: the file work runs as that Bot's user. The shared user without it.
    pub bot: Option<String>,
    pub path: Option<String>,
    pub create_dirs: Option<String>,
}

pub async fn get_raw(
    State(state): State<AppState>,
    Query(query): Query<FileRawQuery>,
) -> Result<Response, ApiError> {
    let who = state.users.who(query.bot.as_deref())?;
    on_disk(who, move || get_raw_now(&state, query)).await?
}

fn get_raw_now(state: &AppState, query: FileRawQuery) -> Result<Response, ApiError> {
    let user_path = query.path.as_deref().unwrap_or("");
    if user_path.is_empty() {
        return Err(ApiError::invalid_request("path is required"));
    }
    let resolved = resolve_in_canonical_jail(&state.workspace, user_path)?;
    let meta = std::fs::metadata(&resolved).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found(format!("not found: {}", resolved.display()))
        } else {
            ApiError::io(err.to_string())
        }
    })?;
    if meta.is_dir() {
        return Err(ApiError::invalid_request(
            "raw GET is for files; list directories with GET /v1/files",
        ));
    }
    if meta.len() > state.max_file_bytes {
        return Err(ApiError::payload_too_large(format!(
            "file is {} bytes; max is {}",
            meta.len(),
            state.max_file_bytes
        )));
    }
    let bytes = std::fs::read(&resolved).map_err(|err| ApiError::io(err.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

pub async fn put_raw(
    State(state): State<AppState>,
    Query(query): Query<FileRawQuery>,
    body: Bytes,
) -> Result<Json<FilePutResponse>, ApiError> {
    let who = state.users.who(query.bot.as_deref())?;
    on_disk(who, move || put_raw_now(&state, query, body)).await?
}

fn put_raw_now(
    state: &AppState,
    query: FileRawQuery,
    body: Bytes,
) -> Result<Json<FilePutResponse>, ApiError> {
    let user_path = query.path.as_deref().unwrap_or("");
    if user_path.is_empty() {
        return Err(ApiError::invalid_request("path is required"));
    }
    let resolved = resolve_in_canonical_jail(&state.workspace, user_path)?;
    let root = resolve_in_canonical_jail(&state.workspace, "")?;
    if resolved == root {
        return Err(ApiError::invalid_request(
            "refusing to overwrite the workspace root",
        ));
    }
    if body.len() as u64 > state.max_file_bytes {
        return Err(ApiError::payload_too_large(format!(
            "payload is {} bytes; max is {}",
            body.len(),
            state.max_file_bytes
        )));
    }
    if resolved.exists() && resolved.is_dir() {
        return Err(ApiError::invalid_request(
            "path is a directory; write a file path",
        ));
    }
    let create_dirs = matches!(
        query
            .create_dirs
            .as_deref()
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        None | Some("1") | Some("true") | Some("yes")
    );
    if create_dirs {
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|err| ApiError::io(err.to_string()))?;
        }
    }
    std::fs::write(&resolved, &body).map_err(|err| ApiError::io(err.to_string()))?;
    Ok(Json(FilePutResponse {
        path: display_under_workspace(&state.workspace, &resolved),
        bytes_written: body.len() as u64,
    }))
}

pub async fn put(
    State(state): State<AppState>,
    Json(req): Json<FilePutRequest>,
) -> Result<Json<FilePutResponse>, ApiError> {
    let who = state.users.who(req.bot.as_deref())?;
    on_disk(who, move || put_now(&state, req)).await?
}

fn put_now(state: &AppState, req: FilePutRequest) -> Result<Json<FilePutResponse>, ApiError> {
    if req.path.is_empty() {
        return Err(ApiError::invalid_request("path is required"));
    }
    let resolved = resolve_in_canonical_jail(&state.workspace, &req.path)?;
    let root = resolve_in_canonical_jail(&state.workspace, "")?;
    if resolved == root {
        return Err(ApiError::invalid_request(
            "refusing to overwrite the workspace root",
        ));
    }

    let encoding = req.encoding.as_deref().unwrap_or("utf8");
    let bytes = if encoding.eq_ignore_ascii_case("utf8") || encoding.eq_ignore_ascii_case("text") {
        req.content.into_bytes()
    } else if encoding.eq_ignore_ascii_case("base64") {
        b64_decode(&req.content).map_err(ApiError::invalid_request)?
    } else {
        return Err(ApiError::invalid_request(format!(
            "unsupported encoding '{encoding}'"
        )));
    };

    if bytes.len() as u64 > state.max_file_bytes {
        return Err(ApiError::payload_too_large(format!(
            "payload is {} bytes; max is {}",
            bytes.len(),
            state.max_file_bytes
        )));
    }

    if resolved.exists() && resolved.is_dir() {
        return Err(ApiError::invalid_request(
            "path is a directory; write a file path",
        ));
    }

    if req.create_dirs.unwrap_or(true) {
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|err| ApiError::io(err.to_string()))?;
        }
    }

    std::fs::write(&resolved, &bytes).map_err(|err| ApiError::io(err.to_string()))?;

    Ok(Json(FilePutResponse {
        path: display_under_workspace(&state.workspace, &resolved),
        bytes_written: bytes.len() as u64,
    }))
}

pub async fn delete(
    State(state): State<AppState>,
    Query(query): Query<FileDeleteQuery>,
) -> Result<Json<FileDeleteResponse>, ApiError> {
    let who = state.users.who(query.bot.as_deref())?;
    on_disk(who, move || delete_now(&state, query)).await?
}

fn delete_now(
    state: &AppState,
    query: FileDeleteQuery,
) -> Result<Json<FileDeleteResponse>, ApiError> {
    let user_path = query.path.as_deref().unwrap_or("");
    if user_path.is_empty() {
        return Err(ApiError::invalid_request("path is required"));
    }
    let resolved = resolve_in_canonical_jail(&state.workspace, user_path)?;
    let root = resolve_in_canonical_jail(&state.workspace, "")?;
    if resolved == root {
        return Err(ApiError::invalid_request(
            "refusing to delete the workspace root",
        ));
    }

    let meta = std::fs::metadata(&resolved).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found(format!("not found: {}", resolved.display()))
        } else {
            ApiError::io(err.to_string())
        }
    })?;

    let recursive = matches!(
        query
            .recursive
            .as_deref()
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("1") | Some("true") | Some("yes")
    );

    if meta.is_dir() {
        if recursive {
            std::fs::remove_dir_all(&resolved).map_err(|err| ApiError::io(err.to_string()))?;
        } else {
            std::fs::remove_dir(&resolved).map_err(|err| {
                if err.kind() == std::io::ErrorKind::DirectoryNotEmpty {
                    ApiError::invalid_request(
                        "directory is not empty; pass recursive=true to delete it",
                    )
                } else {
                    ApiError::io(err.to_string())
                }
            })?;
        }
    } else {
        std::fs::remove_file(&resolved).map_err(|err| ApiError::io(err.to_string()))?;
    }

    Ok(Json(FileDeleteResponse {
        path: display_under_workspace(&state.workspace, &resolved),
        deleted: true,
    }))
}

pub async fn mkdir(
    State(state): State<AppState>,
    Json(req): Json<MkdirRequest>,
) -> Result<Json<MkdirResponse>, ApiError> {
    let who = state.users.who(req.bot.as_deref())?;
    on_disk(who, move || mkdir_now(&state, req)).await?
}

fn mkdir_now(state: &AppState, req: MkdirRequest) -> Result<Json<MkdirResponse>, ApiError> {
    if req.path.is_empty() {
        return Err(ApiError::invalid_request("path is required"));
    }
    let resolved = resolve_in_canonical_jail(&state.workspace, &req.path)?;
    let root = resolve_in_canonical_jail(&state.workspace, "")?;
    if resolved == root {
        return Ok(Json(MkdirResponse {
            path: display_under_workspace(&state.workspace, &resolved),
            created: false,
        }));
    }

    if resolved.exists() {
        if resolved.is_dir() {
            return Ok(Json(MkdirResponse {
                path: display_under_workspace(&state.workspace, &resolved),
                created: false,
            }));
        }
        return Err(ApiError::invalid_request(
            "path exists and is not a directory",
        ));
    }

    if req.parents.unwrap_or(true) {
        std::fs::create_dir_all(&resolved).map_err(|err| ApiError::io(err.to_string()))?;
    } else {
        std::fs::create_dir(&resolved).map_err(|err| ApiError::io(err.to_string()))?;
    }

    Ok(Json(MkdirResponse {
        path: display_under_workspace(&state.workspace, &resolved),
        created: true,
    }))
}

pub async fn rename(
    State(state): State<AppState>,
    Json(req): Json<RenameRequest>,
) -> Result<Json<RenameResponse>, ApiError> {
    let who = state.users.who(req.bot.as_deref())?;
    on_disk(who, move || rename_now(&state, req)).await?
}

fn rename_now(state: &AppState, req: RenameRequest) -> Result<Json<RenameResponse>, ApiError> {
    if req.from.is_empty() || req.to.is_empty() {
        return Err(ApiError::invalid_request("from and to are required"));
    }
    let from = resolve_in_canonical_jail(&state.workspace, &req.from)?;
    let to = resolve_in_canonical_jail(&state.workspace, &req.to)?;
    let root = resolve_in_canonical_jail(&state.workspace, "")?;
    if from == root || to == root {
        return Err(ApiError::invalid_request(
            "refusing to rename the workspace root",
        ));
    }
    if !from.exists() {
        return Err(ApiError::not_found(format!(
            "not found: {}",
            from.display()
        )));
    }
    if to.exists() {
        return Err(ApiError::invalid_request("destination already exists"));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|err| ApiError::io(err.to_string()))?;
    }
    std::fs::rename(&from, &to).map_err(|err| ApiError::io(err.to_string()))?;
    Ok(Json(RenameResponse {
        from: display_under_workspace(&state.workspace, &from),
        to: display_under_workspace(&state.workspace, &to),
    }))
}

fn display_under_workspace(workspace: &Path, resolved: &Path) -> String {
    match resolved.strip_prefix(workspace) {
        Ok(rel) if rel.as_os_str().is_empty() => workspace.display().to_string(),
        Ok(rel) => format!("{}/{}", workspace.display(), rel.display()),
        Err(_) => resolved.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{b64_decode, b64_encode};

    #[test]
    fn base64_roundtrip() {
        let data = b"hello world!";
        let encoded = b64_encode(data);
        assert_eq!(b64_decode(&encoded).unwrap(), data);
        assert_eq!(b64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64_decode("aGVs\nbG8=").unwrap(), b"hello");
    }
}
