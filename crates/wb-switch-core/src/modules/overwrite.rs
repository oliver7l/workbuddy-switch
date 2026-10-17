//! 切号时整包覆盖目标账号的会话列表（归档版默认，删除版可选）。
//!
//! 判定依据（2026-10-17 对 WorkBuddy 客户端源码的实测结论，别凭印象改）：
//! - 所有账号的会话共存在同一份 `workbuddy.db` 的 `sessions` 表里，按 `user_id` 分；
//! - 云端会话列表同步回来时走 `CLOUD_CONVERSATION_UPSERT_SQL`：
//!   `INSERT ... ON CONFLICT(id) DO UPDATE ... WHERE sessions.deleted_at = -1`；
//!   ⇒ **本地硬删（DELETE FROM sessions）必被云端列表同步复活**（行不存在就走
//!   INSERT 分支原样插回），⛔ 禁用；**本地软删（deleted_at=时间戳）不会**（UPDATE
//!   分支的 WHERE 不命中已删行）——官方 App 自身删除就是软删，全程序没有一条
//!   `DELETE FROM sessions`；
//! - `transport='cloud'` 的会话（id 为服务端数字串）会被云端列表 upsert 回灌；
//!   这类会话必须同时调云端 archive / delete，否则下次列表同步又回来；
//! - uuid 形态的本地会话不在云端列表里（2026-10-17 实测列表只含数字 id），
//!   本地软删即足够且永久；
//! - 云端 API（与官方 App 同源）：`POST {api_endpoint}/console/as/conversations/{id}/archive`
//!   （可逆）/ `.../delete`（全局不可逆，所有设备同步消失）。
//!
//! 流程：校验 → 备份 → 本地软删目标账号全部活跃会话 → 云端逐条 archive/delete
//! （仅数字 id 的云会话）→ delete 模式下清理 edge_sync_mapping 行。
//!
//! 复制会话（路径 B）必须在覆盖**之后**执行：覆盖只清理执行时刻的活跃会话，
//! 复制新写入的行不会被本轮清理波及（switch.rs 的编排顺序保证这一点）。

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::modules::account;
use crate::modules::config;
use crate::modules::process;
use crate::modules::refresh;
use crate::modules::session::{
    backup_workbuddy_db, edge_sync_db_path, open_db, session_copy_supported_at, SessionPaths,
};
use crate::modules::variant::WbVariant;

/// 归档模式：云端 archive（可逆）+ 本地软删。默认值。
pub const OVERWRITE_MODE_ARCHIVE: &str = "archive";
/// 删除模式：云端 delete（全局不可逆）+ 本地软删 + 清 edge_sync_mapping 行。
pub const OVERWRITE_MODE_DELETE: &str = "delete";

/// 非法模式的统一文案（列出合法值，方便前端下拉对齐）。
pub const OVERWRITE_INVALID_MODE: &str = "覆盖模式只支持 archive（归档，默认）或 delete（删除）";

/// WorkBuddy 正在运行时的统一文案（与复制路径同口径）。
pub const OVERWRITE_APP_RUNNING: &str =
    "WorkBuddy 正在运行，已阻止修改会话数据；请先退出 WorkBuddy 后重试";

/// 档位不支持会话覆盖时的统一文案（与复制路径同判据）。
pub const OVERWRITE_UNSUPPORTED: &str = "该档位暂不支持会话覆盖";

/// 云端执行器：入参（服务端会话 id, 模式 archive|delete），成功 Ok(())。
/// 单测注入假执行器，生产路径走 [`cloud_archive_or_delete`]。
pub type CloudExecutor<'a> = &'a dyn Fn(&str, &str) -> Result<(), String>;

/// 本地软删 + 云端清理都完成后交给宿主的报告形状：
/// `{ mode, targetUid, scanned, softDeleted, cloudSkipped, cloudFailed, mappingRemoved, backup }`。
pub fn overwrite_target_sessions(target_acc: &Value, mode: &str) -> Result<Value, String> {
    overwrite_target_sessions_with_progress(target_acc, mode, None)
}

/// 带进度回调的覆盖入口（switch.rs 编排时透传切换进度，宿主展示用）。
pub fn overwrite_target_sessions_with_progress(
    target_acc: &Value,
    mode: &str,
    progress: Option<&dyn Fn(&str)>,
) -> Result<Value, String> {
    let variant = account::variant_of(target_acc);
    let paths = SessionPaths::for_variant(variant);
    let account_for_cloud = target_acc.clone();
    let cloud = move |conversation_id: &str, mode: &str| {
        cloud_archive_or_delete(
            account_for_cloud.clone(),
            variant,
            conversation_id.to_string(),
            mode.to_string(),
        )
    };
    overwrite_target_sessions_at(
        &paths,
        variant,
        target_acc,
        mode,
        process::is_workbuddy_running,
        Some(&cloud),
        progress,
    )
}

/// 可注入入口（单测注入临时目录、假运行探针与假云端执行器，不触碰真实路径）。
fn overwrite_target_sessions_at(
    paths: &SessionPaths,
    variant: WbVariant,
    target_acc: &Value,
    mode: &str,
    is_app_running: impl Fn(WbVariant) -> bool,
    cloud: Option<CloudExecutor<'_>>,
    progress: Option<&dyn Fn(&str)>,
) -> Result<Value, String> {
    let mode = normalize_mode(mode)?;
    if is_app_running(variant) {
        return Err(OVERWRITE_APP_RUNNING.to_string());
    }
    if !session_copy_supported_at(&paths.data_root) {
        return Err(format!(
            "{}（档位 {}）",
            OVERWRITE_UNSUPPORTED,
            variant.as_str()
        ));
    }
    let target_uid = account::get_str(target_acc, "uid").unwrap_or_default();
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法覆盖会话".to_string());
    }

    // 与复制/同步同锁：覆盖也是一次档位级会话写入，必须与复制、同步、恢复互斥。
    let _ops_lock = crate::modules::session_link::try_acquire_variant_ops_lock(paths, variant)?;
    if is_app_running(variant) {
        return Err(OVERWRITE_APP_RUNNING.to_string());
    }

    // 备份：主库 + edge-sync 映射库一起快照；备份不可信时不得动手写库。
    let ts = config::utc_iso().replace([':', ' '], "-");
    let backup_dir = paths.backup_root().join(format!("overwrite-{ts}"));
    let backup_db = backup_workbuddy_db(paths, &backup_dir)?;
    let mapping_backup = backup_edge_sync_db(paths, variant, &backup_dir);
    let backup = json!({ "dir": backup_dir.to_string_lossy(), "workbuddyDb": backup_db.to_string_lossy(), "edgeSyncDb": mapping_backup.as_ref().map(|p| p.to_string_lossy().to_string()) });

    let db = paths.workbuddy_db();
    let conn = open_db(&db, false).ok_or_else(|| "会话数据库打开失败".to_string())?;

    // 目标账号当前活跃会话（含 deleted_at=-1 的云端回灌行）。
    let rows: Vec<(String, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, title FROM sessions \
                 WHERE user_id = ?1 AND (deleted_at IS NULL OR deleted_at = -1) \
                 ORDER BY id",
            )
            .map_err(|error| format!("会话查询失败：{error}"))?;
        let iter = stmt
            .query_map([target_uid.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(|error| format!("会话查询失败：{error}"))?;
        iter.flatten().collect()
    };
    let scanned = rows.len();
    if let Some(p) = progress {
        p(&format!("覆盖目标账号会话：本地软删 {scanned} 条…"));
    }

    // 本地软删（幂等）：与官方 App 删除同款——deleted_at/updated_at 写同一时间戳。
    let now_ms = config::now_ms();
    let mut soft_deleted: Vec<Value> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "UPDATE sessions SET deleted_at = ?1, updated_at = ?1 \
                 WHERE id = ?2 AND user_id = ?3 AND (deleted_at IS NULL OR deleted_at = -1)",
            )
            .map_err(|error| format!("软删语句准备失败：{error}"))?;
        conn.execute_batch("BEGIN")
            .map_err(|error| format!("事务开启失败：{error}"))?;
        for (id, title) in &rows {
            match stmt.execute(rusqlite::params![now_ms, id, target_uid]) {
                Ok(1) => {
                    soft_deleted.push(json!({ "id": id, "title": title }));
                }
                Ok(_) => {
                    // 并发下已被他人删除：不算失败，也不重复报。
                }
                Err(error) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!("软删会话 {id} 失败：{error}"));
                }
            }
        }
        conn.execute_batch("COMMIT")
            .map_err(|error| format!("事务提交失败：{error}"))?;
    }

    // 云端清理：只针对会进云端列表的数字 id 会话（transport='cloud'）。
    // uuid 形态的本地会话不在云端列表里（见模块头注释），本地软删已足够。
    let mut cloud_skipped: Vec<Value> = Vec::new();
    let mut cloud_failed: Vec<Value> = Vec::new();
    let mut mapping_removed = 0usize;
    let mapping_conn = open_edge_mapping_db(paths, variant);
    for item in &mut soft_deleted {
        let id = item["id"].as_str().unwrap_or_default();
        let conversation_id = mapping_conversation_id(mapping_conn.as_ref(), id)
            .or_else(|| is_numeric_id(id).then(|| id.to_string()));
        let Some(conversation_id) = conversation_id else {
            cloud_skipped.push(json!({ "id": id, "reason": "非云端会话，本地软删已足够" }));
            continue;
        };
        if !is_numeric_id(&conversation_id) {
            cloud_skipped.push(json!({ "id": id, "conversationId": conversation_id, "reason": "edge-sync 会话无云端列表接口，本地软删已足够" }));
            continue;
        }
        if let Some(p) = progress {
            p(&format!(
                "云端{}会话 {conversation_id}…",
                if mode == OVERWRITE_MODE_DELETE {
                    "删除"
                } else {
                    "归档"
                }
            ));
        }
        let Some(cloud) = cloud else {
            cloud_skipped.push(json!({ "id": id, "conversationId": conversation_id, "reason": "云端清理不可用（离线）" }));
            continue;
        };
        match cloud(&conversation_id, mode) {
            Ok(()) => {
                if mode == OVERWRITE_MODE_DELETE {
                    mapping_removed +=
                        remove_mapping_by_conversation(mapping_conn.as_ref(), &conversation_id);
                }
                item["conversationId"] = json!(conversation_id);
                item["cloud"] = json!(if mode == OVERWRITE_MODE_DELETE {
                    "deleted"
                } else {
                    "archived"
                });
            }
            Err(error) => {
                cloud_failed
                    .push(json!({ "id": id, "conversationId": conversation_id, "error": error }));
            }
        }
    }

    let report = json!({
        "mode": mode,
        "targetUid": target_uid,
        "scanned": scanned,
        "softDeleted": soft_deleted,
        "cloudSkipped": cloud_skipped,
        "cloudFailed": cloud_failed,
        "mappingRemoved": mapping_removed,
        "backup": backup,
    });
    Ok(report)
}

/// 归一化并校验模式参数；接受空串按默认归档处理。
fn normalize_mode(mode: &str) -> Result<&'static str, String> {
    match mode.trim() {
        "" | OVERWRITE_MODE_ARCHIVE => Ok(OVERWRITE_MODE_ARCHIVE),
        OVERWRITE_MODE_DELETE => Ok(OVERWRITE_MODE_DELETE),
        _ => Err(OVERWRITE_INVALID_MODE.to_string()),
    }
}

fn is_numeric_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
}

/// 把 edge-sync 映射库一并备份进本次覆盖的备份目录（存在才备份，失败不阻断）。
fn backup_edge_sync_db(
    paths: &SessionPaths,
    variant: WbVariant,
    backup_dir: &Path,
) -> Option<PathBuf> {
    let src = edge_sync_db_path(&paths.data_root, variant);
    if !src.is_file() {
        return None;
    }
    let name = src.file_name()?;
    let dest = backup_dir.join(name);
    std::fs::copy(&src, &dest).ok()?;
    Some(dest)
}

/// 打开 edge-sync 映射库（读写）；不存在或没有映射表时返回 None（云端清理照常跳过）。
fn open_edge_mapping_db(paths: &SessionPaths, variant: WbVariant) -> Option<Connection> {
    let path = edge_sync_db_path(&paths.data_root, variant);
    if !path.is_file() {
        return None;
    }
    let conn = open_db(&path, false)?;
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='edge_sync_mapping')",
            [],
            |row| row.get(0),
        )
        .ok()?;
    exists.then_some(conn)
}

/// 查会话在 edge-sync 映射里的服务端会话 id。
fn mapping_conversation_id(conn: Option<&Connection>, session_id: &str) -> Option<String> {
    let conn = conn?;
    conn.query_row(
        "SELECT conversation_id FROM edge_sync_mapping WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// 按服务端会话 id 清理映射行（delete 模式云删成功后调用），返回删除行数。
fn remove_mapping_by_conversation(conn: Option<&Connection>, conversation_id: &str) -> usize {
    let Some(conn) = conn else { return 0 };
    conn.execute(
        "DELETE FROM edge_sync_mapping WHERE conversation_id = ?1",
        [conversation_id],
    )
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 云端执行器
// ---------------------------------------------------------------------------

/// 真实云端执行器：POST {endpoint}/console/as/conversations/{id}/archive|delete。
///
/// 每次调用在独立线程上起 current-thread runtime——switch_account 跑在宿主的
/// spawn_blocking 线程里，直接 block_on 会踩「runtime 内再起 runtime」的坑；
/// 独立线程两种宿主（Tauri / HTTP server）下都安全。
fn cloud_archive_or_delete(
    account: Value,
    variant: WbVariant,
    conversation_id: String,
    mode: String,
) -> Result<(), String> {
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("异步运行时创建失败：{error}"))?;
            rt.block_on(cloud_call_async(account, variant, conversation_id, mode))
        });
        handle
            .join()
            .unwrap_or_else(|_| Err("云端清理线程崩溃".to_string()))
    })
}

async fn cloud_call_async(
    account: Value,
    variant: WbVariant,
    conversation_id: String,
    mode: String,
) -> Result<(), String> {
    let action = if mode == OVERWRITE_MODE_DELETE {
        "delete"
    } else {
        "archive"
    };
    let url = format!(
        "{}/console/as/conversations/{}/{}",
        variant.api_endpoint(),
        conversation_id,
        action
    );
    let headers = account::build_auth_headers(&account);
    let body = (mode == OVERWRITE_MODE_DELETE).then(|| json!({ "id": conversation_id }));
    let (status, _, text) =
        config::http_request_raw(&url, "POST", body, Some(&headers), None, true).await;
    // 404 = 服务端本就没有这个会话，视为已达成。
    if status == 404 {
        return Ok(());
    }
    if status == 401 || status == 403 {
        // token 失效：刷新一次后重试（与签到链路同策略）。
        let refreshed = refresh::refresh_account_token(account.clone()).await;
        let still_broken = refreshed
            .get("needs_relogin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !still_broken {
            let headers = account::build_auth_headers(&refreshed);
            let body = (mode == OVERWRITE_MODE_DELETE).then(|| json!({ "id": conversation_id }));
            let (status, _, text) =
                config::http_request_raw(&url, "POST", body, Some(&headers), None, true).await;
            return finish_cloud_response(status, &text, action);
        }
        return Err("登录态已失效且刷新失败（needs_relogin），未执行云端操作".to_string());
    }
    finish_cloud_response(status, &text, action)
}

fn finish_cloud_response(status: u16, text: &str, action: &str) -> Result<(), String> {
    if !(200..300).contains(&status) {
        return Err(format!(
            "云端{action}失败：HTTP {status} {}",
            truncate(text, 200)
        ));
    }
    // 网关成功但业务码非 0 的形态：{code: !=0, msg}。
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        let code = value.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            let msg = value
                .get("msg")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(format!("云端{action}失败：code={code} {msg}"));
        }
    }
    Ok(())
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! 离线单测：临时目录建库 + 假云端执行器，覆盖「软删口径 / 复活防护 /
    //! 归属隔离 / 云端只打数字 id / mapping 清理 / 模式校验 / 运行拦截」。

    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb_switch_overwrite_{name}_{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup_paths(home: &Path) -> SessionPaths {
        let data_root = home.join(".workbuddy");
        std::fs::create_dir_all(data_root.join("projects")).unwrap();
        SessionPaths {
            store_root: home.join(".wb-switch-store"),
            data_root,
            link_namespace: crate::modules::session::LinkNamespace::WorkBuddy,
            auth_file: home.join("workbuddy-desktop.info"),
        }
    }

    const UID_A: &str = "uid-aaaaaaaa-0000-0000-0000-000000000001";
    const UID_B: &str = "uid-bbbbbbbb-0000-0000-0000-000000000002";

    fn seed_sessions(paths: &SessionPaths) {
        let conn = open_db(&paths.workbuddy_db(), false).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                user_id TEXT,
                title TEXT,
                updated_at INTEGER,
                deleted_at INTEGER,
                transport TEXT DEFAULT 'local'
            );",
        )
        .unwrap();
        let insert = |id: &str, uid: &str, title: &str, deleted_at: Option<i64>| {
            conn.execute(
                "INSERT INTO sessions (id, user_id, title, updated_at, deleted_at) VALUES (?1, ?2, ?3, 1, ?4)",
                rusqlite::params![id, uid, title, deleted_at],
            )
            .unwrap();
        };
        // B 的活跃行：uuid 本地会话、数字云端会话、deleted_at=-1 的回灌行、已删行。
        insert("uuid-1", UID_B, "本地会话", None);
        insert("200123", UID_B, "云端会话", None);
        insert("300456", UID_B, "云端回灌行", Some(-1));
        insert("400789", UID_B, "早就删掉的", Some(12345));
        // A 的行：绝不能被波及。
        insert("uuid-a1", UID_A, "A 的会话", None);
    }

    fn seed_mapping(paths: &SessionPaths) {
        let db = edge_sync_db_path(&paths.data_root, WbVariant::Cn);
        let conn = open_db(&db, false).unwrap();
        conn.execute_batch(
            "CREATE TABLE edge_sync_mapping (
                session_id TEXT PRIMARY KEY,
                conversation_id TEXT,
                msg_channel TEXT DEFAULT '',
                created_at INTEGER DEFAULT 0
            );",
        )
        .unwrap();
        for (sid, cid) in [
            ("uuid-1", "uuid-1"),
            ("200123", "200123"),
            ("300456", "300456"),
        ] {
            conn.execute(
                "INSERT INTO edge_sync_mapping (session_id, conversation_id) VALUES (?1, ?2)",
                [sid, cid],
            )
            .unwrap();
        }
    }

    fn account_for(uid: &str) -> Value {
        json!({ "uid": uid, "nickname": "目标", "access_token": "tok", "refresh_token": "rt" })
    }

    fn not_running(_: WbVariant) -> bool {
        false
    }

    /// 记录型假云端执行器：(conversation_id, mode) 调用流水。
    type CloudLog = Mutex<Vec<(String, String)>>;

    fn fake_cloud_ok(log: &CloudLog) -> impl Fn(&str, &str) -> Result<(), String> + '_ {
        move |cid: &str, mode: &str| {
            log.lock()
                .unwrap()
                .push((cid.to_string(), mode.to_string()));
            Ok(())
        }
    }

    #[test]
    fn archive_mode_soft_deletes_active_only_and_archives_numeric_ids() {
        let tmp = TempDir::new("archive");
        let paths = setup_paths(&tmp.0);
        seed_sessions(&paths);
        seed_mapping(&paths);
        let log = CloudLog::default();
        let cloud = fake_cloud_ok(&log);

        let report = overwrite_target_sessions_at(
            &paths,
            WbVariant::Cn,
            &account_for(UID_B),
            OVERWRITE_MODE_ARCHIVE,
            not_running,
            Some(&cloud),
            None,
        )
        .unwrap();

        assert_eq!(report["mode"], OVERWRITE_MODE_ARCHIVE);
        assert_eq!(
            report["scanned"].as_i64(),
            Some(3),
            "活跃行 = uuid-1 + 200123 + 300456"
        );
        assert_eq!(report["softDeleted"].as_array().unwrap().len(), 3);

        let conn = open_db(&paths.workbuddy_db(), false).unwrap();
        let deleted_at = |id: &str| -> Option<i64> {
            conn.query_row(
                "SELECT deleted_at FROM sessions WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            deleted_at("uuid-1").unwrap_or(0) > 1,
            "uuid 本地会话必须软删"
        );
        assert!(deleted_at("200123").unwrap_or(0) > 1);
        assert!(
            deleted_at("300456").unwrap_or(-9) > 1,
            "deleted_at=-1 的回灌行也要清"
        );
        assert_eq!(deleted_at("400789"), Some(12345), "已删行不得被碰");
        assert_eq!(deleted_at("uuid-a1"), None, "来源账号的行绝不能被波及");

        // 云端只打数字 id：200123 与 300456 两条。
        let calls = log.lock().unwrap();
        let mut ids: Vec<&str> = calls.iter().map(|(cid, _)| cid.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["200123", "300456"]);
        assert!(calls.iter().all(|(_, mode)| mode == OVERWRITE_MODE_ARCHIVE));

        // 归档模式保留 mapping（可恢复）。
        let mapping = open_db(&edge_sync_db_path(&paths.data_root, WbVariant::Cn), false).unwrap();
        let count: i64 = mapping
            .query_row("SELECT COUNT(*) FROM edge_sync_mapping", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 3, "归档模式不得清理 mapping");
    }

    #[test]
    fn delete_mode_removes_mapping_after_cloud_delete() {
        let tmp = TempDir::new("delete");
        let paths = setup_paths(&tmp.0);
        seed_sessions(&paths);
        seed_mapping(&paths);
        let log = CloudLog::default();
        let cloud = fake_cloud_ok(&log);

        let report = overwrite_target_sessions_at(
            &paths,
            WbVariant::Cn,
            &account_for(UID_B),
            OVERWRITE_MODE_DELETE,
            not_running,
            Some(&cloud),
            None,
        )
        .unwrap();
        assert_eq!(
            report["mappingRemoved"].as_i64(),
            Some(2),
            "两条数字 id 会话的 mapping 被清"
        );
        assert_eq!(report["cloudFailed"].as_array().unwrap().len(), 0);

        let mapping = open_db(&edge_sync_db_path(&paths.data_root, WbVariant::Cn), false).unwrap();
        let left: i64 = mapping
            .query_row(
                "SELECT COUNT(*) FROM edge_sync_mapping WHERE conversation_id IN ('200123','300456')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "云删成功的会话不得残留 mapping");
        let uuid_left: i64 = mapping
            .query_row(
                "SELECT COUNT(*) FROM edge_sync_mapping WHERE conversation_id = 'uuid-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(uuid_left, 1, "uuid 会话的 mapping 不在云端清理范围内");
    }

    #[test]
    fn cloud_failure_is_reported_and_keeps_mapping() {
        let tmp = TempDir::new("cloudfail");
        let paths = setup_paths(&tmp.0);
        seed_sessions(&paths);
        seed_mapping(&paths);
        let failing = |cid: &str, _mode: &str| -> Result<(), String> {
            if cid == "200123" {
                Err("云端删除失败：HTTP 500 boom".to_string())
            } else {
                Ok(())
            }
        };
        let report = overwrite_target_sessions_at(
            &paths,
            WbVariant::Cn,
            &account_for(UID_B),
            OVERWRITE_MODE_DELETE,
            not_running,
            Some(&failing),
            None,
        )
        .unwrap();
        let failed = report["cloudFailed"].as_array().unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["conversationId"], "200123");
        // 本地软删不受云端失败影响。
        assert_eq!(report["softDeleted"].as_array().unwrap().len(), 3);
        let mapping = open_db(&edge_sync_db_path(&paths.data_root, WbVariant::Cn), false).unwrap();
        let left: i64 = mapping
            .query_row(
                "SELECT COUNT(*) FROM edge_sync_mapping WHERE conversation_id = '200123'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 1, "云删失败的 mapping 必须保留以便重试");
    }

    #[test]
    fn invalid_mode_and_app_running_are_rejected() {
        let tmp = TempDir::new("guard");
        let paths = setup_paths(&tmp.0);
        seed_sessions(&paths);
        let err = overwrite_target_sessions_at(
            &paths,
            WbVariant::Cn,
            &account_for(UID_B),
            "replace",
            not_running,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, OVERWRITE_INVALID_MODE);

        let err = overwrite_target_sessions_at(
            &paths,
            WbVariant::Cn,
            &account_for(UID_B),
            OVERWRITE_MODE_ARCHIVE,
            |_| true,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, OVERWRITE_APP_RUNNING);
    }

    #[test]
    fn missing_uid_is_rejected() {
        let tmp = TempDir::new("nouid");
        let paths = setup_paths(&tmp.0);
        seed_sessions(&paths);
        let err = overwrite_target_sessions_at(
            &paths,
            WbVariant::Cn,
            &json!({ "nickname": "没 uid" }),
            OVERWRITE_MODE_ARCHIVE,
            not_running,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("uid"), "{err}");
    }

    #[test]
    fn backup_snapshot_is_written() {
        let tmp = TempDir::new("backup");
        let paths = setup_paths(&tmp.0);
        seed_sessions(&paths);
        let report = overwrite_target_sessions_at(
            &paths,
            WbVariant::Cn,
            &account_for(UID_B),
            OVERWRITE_MODE_ARCHIVE,
            not_running,
            None,
            None,
        )
        .unwrap();
        let dir = report["backup"]["dir"].as_str().unwrap();
        assert!(
            Path::new(dir).join("workbuddy.db").is_file(),
            "主库快照必须存在"
        );
    }

    #[test]
    fn empty_mode_defaults_to_archive() {
        let tmp = TempDir::new("defaultmode");
        let paths = setup_paths(&tmp.0);
        seed_sessions(&paths);
        let log = CloudLog::default();
        let cloud = fake_cloud_ok(&log);
        let report = overwrite_target_sessions_at(
            &paths,
            WbVariant::Cn,
            &account_for(UID_B),
            "",
            not_running,
            Some(&cloud),
            None,
        )
        .unwrap();
        assert_eq!(report["mode"], OVERWRITE_MODE_ARCHIVE);
    }
}
