//! `GET /v1/usage`（及 `/cc/v1/usage`）：客户端用调模型的同一把 Key 查还剩多少
//!
//! 回包照《用量接口 v1》：`object = "usage"`、`version = 1`。
//!
//! - `key`：这把 Key 自己的额度。
//! - `pool`：这把 Key 能用的 Kiro 账号剩余积分之和。只给开了「能看池子」的 Key；
//!   没开的 Key 回包里不出现任何能推算出池子大小的数。
//! - 顶层的 `isValid` / `status` / `planName` / `remaining` / `unit` / `quota`
//!   给 CC Switch 的「Sub2API」用量模板读。
//!
//! 不经过 `auth_middleware`：查询不计调用次数、不占额度；Key 停用或额度用完时也回 200，
//! 用 `state` 说明。

use std::collections::HashMap;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;

use crate::admin::balance_board::BoardEntry;
use crate::admin::client_keys::ClientKey;
use crate::common::auth;
use crate::kiro::token_manager::group_matches;

use super::middleware::AppState;

/// 规范里的单位（程序读）
const UNIT: &str = "credits";
/// 给人看的单位（CC Switch 显示）
const UNIT_LABEL: &str = "积分";
/// 池子余额还没查到时，建议多久后再查
const POOL_RETRY_AFTER_SECS: u64 = 60;
/// 账号因额度用完被停用时的 `disabled_reason`
const QUOTA_EXCEEDED: &str = "QuotaExceeded";

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageResponse {
    pub object: &'static str,
    pub version: u32,
    /// `active` / `disabled` / `exhausted` / `expired`
    pub state: &'static str,
    /// 这份数的时间：Key 的数是实时的，所以就是这次查询的时间
    pub updated_at: String,
    pub key: KeyBlock,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolBlock>,
    pub stats: Stats,

    // ---- CC Switch「Sub2API」模板读的几项 ----
    pub is_valid: bool,
    /// 正常 / 已停用 / 额度已用完 / 已过期
    pub status: &'static str,
    pub plan_name: String,
    /// 这把 Key 下一次调用受限的那个数：Key 限额填 Key 的，不限额又能看池子填池子的
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    pub unit: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KeyBlock {
    pub name: String,
    pub unit: &'static str,
    /// 不限额时不给
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    pub used: f64,
    /// 不限额时不给
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    /// Key 的额度不会自动重置（管理员「重置统计」才清零）
    pub resets_at: Option<String>,
    /// Key 没有过期时间
    pub expires_at: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PoolBlock {
    pub unit: &'static str,
    pub limit: f64,
    pub used: f64,
    pub remaining: f64,
    /// 池子里最早一个账号的下次重置时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// 计入的余额里最旧的那份是什么时候查到的
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_used_at: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    pub used: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    pub unit: &'static str,
}

/// GET /v1/usage（及 /cc/v1/usage）
pub async fn get_key_usage(State(state): State<AppState>, request: Request<Body>) -> Response {
    let key = match (&state.client_keys, auth::extract_api_key(&request)) {
        (Some(mgr), Some(presented)) => mgr.lookup(&presented),
        _ => None,
    };
    let Some(key) = key else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_key",
            "API Key 无效或已作废",
        );
    };

    let now = Utc::now();
    let pool = if key.pool_visible {
        match visible_pool(&state, key.group.as_deref(), now) {
            PoolLookup::Known(pool) => Some(pool),
            // 没有余额表（未启用 Admin API）：只回这把 Key 自己的数
            PoolLookup::Unsupported => None,
            // 规范：从来没查到过回 503。但 Key 自己限额、停用或用完时，Key 的状态和额度
            // 照样有意义，回 200 不带 pool；只有不限额的正常 Key（能显示的只有池子）才回 503
            PoolLookup::NotYetKnown if pool_is_all_there_is(&key) => {
                let mut resp = error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool_unavailable",
                    "池子余额还没查到，请稍后再试",
                );
                resp.headers_mut().insert(
                    header::RETRY_AFTER,
                    HeaderValue::from(POOL_RETRY_AFTER_SECS),
                );
                return resp;
            }
            PoolLookup::NotYetKnown => None,
        }
    } else {
        None
    };

    json_response(StatusCode::OK, &usage_response(&key, pool, now))
}

/// 不限额、未停用、未用完：回包里唯一有用的数就是池子
fn pool_is_all_there_is(key: &ClientKey) -> bool {
    key.max_credits.is_none() && !key.disabled
}

/// 查池子的结果
#[derive(Debug, PartialEq)]
pub enum PoolLookup {
    Known(PoolBlock),
    /// 有账号，但一份余额都还没查到过
    NotYetKnown,
    /// 这个实例没有余额表（未启用 Admin API，余额后台刷新不运行）
    Unsupported,
}

/// 池子里的一个账号
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolMember {
    pub id: u64,
    /// 因额度用完被停用：还算在池子里（到期会自动恢复），但一点都不剩
    pub quota_exhausted: bool,
}

/// 这把 Key 能用的账号的余额之和
fn visible_pool(state: &AppState, group: Option<&str>, now: DateTime<Utc>) -> PoolLookup {
    let (Some(provider), Some(board)) = (&state.kiro_provider, &state.balance_board) else {
        return PoolLookup::Unsupported;
    };
    // 分组规则与调度一致：Key 不绑分组时算全部账号。
    // 手动停用、失效的账号不算；额度用完被停用的算，剩余记 0
    let members: Vec<PoolMember> = provider
        .token_manager()
        .snapshot()
        .entries
        .iter()
        .filter(|c| group_matches(&c.groups, group))
        .filter_map(|c| {
            let quota_exhausted =
                c.disabled && c.disabled_reason.as_deref() == Some(QUOTA_EXCEEDED);
            (!c.disabled || quota_exhausted).then_some(PoolMember {
                id: c.id,
                quota_exhausted,
            })
        })
        .collect();
    match pool_balance(&members, &board.snapshot(), now) {
        Some(pool) => PoolLookup::Known(pool),
        None => PoolLookup::NotYetKnown,
    }
}

pub fn usage_response(
    key: &ClientKey,
    pool: Option<PoolBlock>,
    now: DateTime<Utc>,
) -> UsageResponse {
    let used = round2(key.total_credits);
    let limit = key.max_credits.map(round2);
    let key_remaining = key
        .max_credits
        .map(|limit| round2((limit - key.total_credits).max(0.0)));
    let exhausted = key
        .max_credits
        .is_some_and(|limit| key.total_credits >= limit);
    let (state, status) = if key.disabled {
        ("disabled", "已停用")
    } else if exhausted {
        ("exhausted", "额度已用完")
    } else {
        ("active", "正常")
    };

    // CC Switch 只显示一个数：Key 自己限额就是 Key 的数，否则能看池子就是池子的数
    let (remaining, quota, plan_name) = match (limit, &pool) {
        (Some(limit), _) => (
            key_remaining,
            Some(Quota {
                used,
                limit: Some(limit),
                unit: UNIT_LABEL,
            }),
            key.name.clone(),
        ),
        (None, Some(pool)) => (
            Some(pool.remaining),
            Some(Quota {
                used: pool.used,
                limit: Some(pool.limit),
                unit: UNIT_LABEL,
            }),
            format!("{} · 池子", key.name),
        ),
        (None, None) => (None, None, format!("{} · 不限额", key.name)),
    };

    UsageResponse {
        object: "usage",
        version: 1,
        state,
        updated_at: iso_z(now),
        key: KeyBlock {
            name: key.name.clone(),
            unit: UNIT,
            limit,
            used,
            remaining: key_remaining,
            resets_at: None,
            expires_at: None,
        },
        pool,
        stats: Stats {
            calls: key.total_calls,
            input_tokens: key.total_input_tokens,
            output_tokens: key.total_output_tokens,
            last_used_at: key
                .last_used_at
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| iso_z(t.with_timezone(&Utc))),
        },
        is_valid: state == "active",
        status,
        plan_name,
        remaining,
        unit: UNIT_LABEL,
        quota,
    }
}

/// 汇总这些账号的余额。
///
/// 一个账号都没有时池子就是 0；有账号但一份余额都没查到过时返回 None（回 503）。
pub fn pool_balance(
    members: &[PoolMember],
    board: &HashMap<u64, BoardEntry>,
    now: DateTime<Utc>,
) -> Option<PoolBlock> {
    let balances: Vec<(&BoardEntry, bool)> = members
        .iter()
        .filter_map(|m| board.get(&m.id).map(|b| (b, m.quota_exhausted)))
        .collect();
    if balances.is_empty() && !members.is_empty() {
        return None;
    }
    let (mut limit, mut used, mut remaining) = (0.0, 0.0, 0.0);
    for (b, quota_exhausted) in &balances {
        limit += b.usage_limit;
        if *quota_exhausted {
            // 缓存可能是用完之前查的，以停用状态为准
            used += b.current_usage.max(b.usage_limit);
        } else {
            used += b.current_usage;
            remaining += b.remaining.max(0.0);
        }
    }
    // 已经过去的重置时间是旧缓存留下的（下一次重置还没查到），不能拿来显示
    let now_secs = now.timestamp() as f64;
    let resets_at = balances
        .iter()
        .filter_map(|(b, _)| b.next_reset_at)
        .filter(|t| *t > now_secs)
        .min_by(f64::total_cmp)
        .and_then(unix_to_iso_z);
    // 只按真正用到缓存数的账号算：额度用完的账号剩余按当前停用状态记 0，不看它的旧缓存
    let updated_at = balances
        .iter()
        .filter(|(_, quota_exhausted)| !quota_exhausted)
        .map(|(b, _)| b.cached_at)
        .min_by(f64::total_cmp)
        .and_then(unix_to_iso_z);
    Some(PoolBlock {
        unit: UNIT,
        limit: round2(limit),
        used: round2(used),
        remaining: round2(remaining),
        resets_at,
        updated_at,
    })
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => (
            status,
            [
                (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    object: &'static str,
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

/// `{"object":"error","error":{"code":"…","message":"…"}}`
fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    json_response(
        status,
        &ErrorBody {
            object: "error",
            error: ErrorDetail { code, message },
        },
    )
}

/// 规范要求数字最多两位小数
fn round2(v: f64) -> f64 {
    let r = (v * 100.0).round() / 100.0;
    if r == 0.0 { 0.0 } else { r }
}

fn iso_z(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn unix_to_iso_z(secs: f64) -> Option<String> {
    Utc.timestamp_opt(secs as i64, 0).single().map(iso_z)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(total_credits: f64, max_credits: Option<f64>, disabled: bool) -> ClientKey {
        serde_json::from_value(serde_json::json!({
            "id": 7,
            "name": "张三",
            "key": "sk-test",
            "disabled": disabled,
            "createdAt": "2026-11-01T00:00:00Z",
            "lastUsedAt": "2026-11-06T09:30:15.123456+00:00",
            "totalCalls": 3,
            "totalInputTokens": 100,
            "totalOutputTokens": 20,
            "totalCredits": total_credits,
            "maxCredits": max_credits,
        }))
        .unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 7, 1, 20, 0).unwrap()
    }

    fn entry(remaining: f64, limit: f64, reset: f64, cached_at: f64) -> BoardEntry {
        BoardEntry {
            remaining,
            usage_limit: limit,
            current_usage: limit - remaining,
            next_reset_at: Some(reset),
            cached_at,
        }
    }

    fn m(id: u64) -> PoolMember {
        PoolMember {
            id,
            quota_exhausted: false,
        }
    }

    fn pool(remaining: f64, limit: f64) -> PoolBlock {
        PoolBlock {
            unit: UNIT,
            limit,
            used: limit - remaining,
            remaining,
            resets_at: Some("2026-12-01T00:00:00Z".into()),
            updated_at: Some("2026-11-07T01:18:00Z".into()),
        }
    }

    #[test]
    fn limited_key_reports_its_own_quota() {
        let r = usage_response(&key(12.34567, Some(100.0), false), None, now());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["object"], "usage");
        assert_eq!(json["version"], 1);
        assert_eq!(json["state"], "active");
        assert_eq!(json["updatedAt"], "2026-11-07T01:20:00Z");
        assert_eq!(json["key"]["unit"], "credits");
        assert_eq!(json["key"]["limit"], 100.0);
        assert_eq!(json["key"]["used"], 12.35);
        assert_eq!(json["key"]["remaining"], 87.65);
        assert!(json["key"]["resetsAt"].is_null());
        assert!(json["key"]["expiresAt"].is_null());
        assert!(json.get("pool").is_none());
        assert_eq!(json["stats"]["calls"], 3);
        assert_eq!(json["stats"]["lastUsedAt"], "2026-11-06T09:30:15Z");
        // CC Switch
        assert_eq!(json["isValid"], true);
        assert_eq!(json["status"], "正常");
        assert_eq!(json["planName"], "张三");
        assert_eq!(json["remaining"], 87.65);
        assert_eq!(json["unit"], "积分");
        assert_eq!(
            json["quota"],
            serde_json::json!({ "used": 12.35, "limit": 100.0, "unit": "积分" })
        );
    }

    #[test]
    fn limited_key_with_pool_still_reports_key_numbers_at_top_level() {
        let r = usage_response(
            &key(10.0, Some(100.0), false),
            Some(pool(1950.0, 2000.0)),
            now(),
        );
        assert_eq!(r.remaining, Some(90.0));
        assert_eq!(r.quota.as_ref().unwrap().limit, Some(100.0));
        assert_eq!(r.pool.as_ref().unwrap().remaining, 1950.0);
        assert_eq!(r.plan_name, "张三");
    }

    #[test]
    fn unlimited_key_without_pool_has_no_limit_or_remaining() {
        let r = usage_response(&key(5.0, None, false), None, now());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["state"], "active");
        assert!(json["key"].get("limit").is_none());
        assert!(json["key"].get("remaining").is_none());
        assert_eq!(json["key"]["used"], 5.0);
        assert!(json.get("pool").is_none());
        assert!(json.get("remaining").is_none());
        assert!(json.get("quota").is_none());
        assert_eq!(json["planName"], "张三 · 不限额");
    }

    #[test]
    fn unlimited_key_with_pool_reports_pool_at_top_level() {
        let r = usage_response(&key(5.0, None, false), Some(pool(1950.0, 2000.0)), now());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["pool"]["remaining"], 1950.0);
        assert_eq!(json["pool"]["unit"], "credits");
        assert_eq!(json["pool"]["resetsAt"], "2026-12-01T00:00:00Z");
        assert_eq!(json["remaining"], 1950.0);
        assert_eq!(
            json["quota"],
            serde_json::json!({ "used": 50.0, "limit": 2000.0, "unit": "积分" })
        );
        assert_eq!(json["planName"], "张三 · 池子");
    }

    #[test]
    fn disabled_and_exhausted_keys_are_invalid() {
        let disabled = usage_response(&key(1.0, Some(10.0), true), None, now());
        assert!(!disabled.is_valid);
        assert_eq!(disabled.state, "disabled");
        assert_eq!(disabled.status, "已停用");

        let exhausted = usage_response(&key(10.5, Some(10.0), false), None, now());
        assert!(!exhausted.is_valid);
        assert_eq!(exhausted.state, "exhausted");
        assert_eq!(exhausted.status, "额度已用完");
        assert_eq!(exhausted.remaining, Some(0.0));
        assert_eq!(exhausted.key.remaining, Some(0.0));

        // 上限 0：一次都不能用
        let zero = usage_response(&key(0.0, Some(0.0), false), None, now());
        assert_eq!(zero.state, "exhausted");
        assert_eq!(zero.key.limit, Some(0.0));
    }

    #[test]
    fn pool_sums_the_given_credentials() {
        let board = HashMap::from([
            (1, entry(1500.0, 2000.0, 1_796_083_200.0, 1_793_900_000.0)),
            (2, entry(40.004, 50.0, 1_795_000_000.0, 1_793_800_000.0)),
            (4, entry(300.0, 500.0, 1_796_000_000.0, 1_793_900_000.0)),
        ]);
        // 5 还没查到余额：按已查到的算
        let pool = pool_balance(&[m(1), m(2), m(5)], &board, now()).unwrap();
        assert_eq!(pool.remaining, 1540.0);
        assert_eq!(pool.limit, 2050.0);
        assert_eq!(pool.used, 510.0);
        assert_eq!(pool.resets_at.as_deref(), Some("2026-11-18T11:06:40Z"));
        assert_eq!(pool.updated_at.as_deref(), Some("2026-11-04T13:46:40Z"));
    }

    #[test]
    fn pool_is_unavailable_until_any_balance_is_known() {
        assert_eq!(pool_balance(&[m(1)], &HashMap::new(), now()), None);
        // 一个账号都没有：池子就是 0
        let empty = pool_balance(&[], &HashMap::new(), now()).unwrap();
        assert_eq!((empty.limit, empty.used, empty.remaining), (0.0, 0.0, 0.0));
    }

    #[test]
    fn negative_remaining_counts_as_zero() {
        let board = HashMap::from([(1, entry(-3.0, 50.0, 1.0, 1.0))]);
        assert_eq!(pool_balance(&[m(1)], &board, now()).unwrap().remaining, 0.0);
    }

    #[test]
    fn quota_exhausted_account_counts_as_fully_used() {
        // 缓存是用完之前查的（还剩 5），账号已因额度用完停用
        let board = HashMap::from([
            (1, entry(5.0, 50.0, 1_795_000_000.0, 1.0)),
            (2, entry(30.0, 50.0, 1_796_000_000.0, 1.0)),
        ]);
        let exhausted = PoolMember {
            id: 1,
            quota_exhausted: true,
        };
        let pool = pool_balance(&[exhausted, m(2)], &board, now()).unwrap();
        assert_eq!(pool.limit, 100.0);
        assert_eq!(pool.used, 70.0);
        assert_eq!(pool.remaining, 30.0);
        assert_eq!(pool.resets_at.as_deref(), Some("2026-11-18T11:06:40Z"));
    }

    #[test]
    fn stale_reset_and_exhausted_cache_times_are_not_reported() {
        // #1：10 月 20 日查的、11 月 1 日重置，之后额度用完被停用；#2：刚查过、12 月 1 日重置
        let board = HashMap::from([
            (1, entry(0.0, 50.0, 1_793_491_200.0, 1_792_454_400.0)),
            (2, entry(30.0, 50.0, 1_796_083_200.0, 1_794_014_400.0)),
        ]);
        let exhausted = PoolMember {
            id: 1,
            quota_exhausted: true,
        };
        let pool = pool_balance(&[exhausted, m(2)], &board, now()).unwrap();
        assert_eq!(pool.resets_at.as_deref(), Some("2026-12-01T00:00:00Z"));
        assert_eq!(pool.updated_at.as_deref(), Some("2026-11-07T01:20:00Z"));

        // 只剩用完的账号：重置时间已过、缓存时间也不作数，两项都不给
        let only = pool_balance(&[exhausted], &board, now()).unwrap();
        assert_eq!(only.resets_at, None);
        assert_eq!(only.updated_at, None);
        assert_eq!(only.remaining, 0.0);
    }

    #[test]
    fn only_unlimited_active_keys_need_the_pool() {
        assert!(pool_is_all_there_is(&key(1.0, None, false)));
        assert!(!pool_is_all_there_is(&key(1.0, Some(10.0), false)));
        assert!(!pool_is_all_there_is(&key(1.0, None, true)));
    }

    #[test]
    fn error_body_shape() {
        let body = serde_json::to_string(&ErrorBody {
            object: "error",
            error: ErrorDetail {
                code: "invalid_key",
                message: "x",
            },
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"object":"error","error":{"code":"invalid_key","message":"x"}}"#
        );
    }
}
