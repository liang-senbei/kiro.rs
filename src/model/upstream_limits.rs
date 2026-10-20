//! 上游模型上下文窗口注册表
//!
//! Kiro 的流里只有 `contextUsagePercentage`，换算成 token 数要乘以该模型的
//! 上下文窗口；上游 `ListAvailableModels` 的 `tokenLimits.maxInputTokens`
//! 就是这个窗口。`MultiTokenManager` 每拿到一份凭据的模型列表就调用
//! [`record`] 记下来，`get_context_window_size` 这类无状态自由函数用
//! [`max_input_tokens`] 同步查询（仿 `custom_models` 的全局注册表惯例）。
//!
//! 与 `model_cache` 分开存放：切换代理、修改自定义模型时 `model_cache` 会整表
//! 清空，而业务请求不会重新拉模型列表（Claude Code 也不调 `/v1/models`）。
//! 窗口是模型本身的属性，不该随这些状态一起丢；只在凭据删除时 [`forget`]。

use std::collections::HashMap;

use crate::kiro::model::available_models::ListAvailableModelsResponse;

/// 凭据 id -> (小写模型 id -> maxInputTokens)
type Limits = HashMap<u64, HashMap<String, i32>>;

// 测试并行运行且各自伪造模型列表，按线程隔离，避免互相污染窗口查询。
#[cfg(not(test))]
static LIMITS: std::sync::RwLock<Option<Limits>> = std::sync::RwLock::new(None);

#[cfg(test)]
thread_local! {
    static LIMITS: std::cell::RefCell<Limits> = std::cell::RefCell::new(HashMap::new());
}

#[cfg(not(test))]
fn read<R>(f: impl FnOnce(&Limits) -> R) -> Option<R> {
    let guard = LIMITS.read().ok()?;
    guard.as_ref().map(f)
}

#[cfg(not(test))]
fn write(f: impl FnOnce(&mut Limits)) {
    if let Ok(mut guard) = LIMITS.write() {
        f(guard.get_or_insert_with(HashMap::new));
    }
}

#[cfg(test)]
fn read<R>(f: impl FnOnce(&Limits) -> R) -> Option<R> {
    Some(LIMITS.with(|limits| f(&limits.borrow())))
}

#[cfg(test)]
fn write(f: impl FnOnce(&mut Limits)) {
    LIMITS.with(|limits| f(&mut limits.borrow_mut()));
}

/// 记录某个凭据最新一次拿到的模型窗口。
///
/// 整份替换该凭据的旧记录；响应里一个有效 `maxInputTokens` 都没有时保留旧值，
/// 免得上游偶发省略 `tokenLimits` 就把已知窗口抹掉。
pub fn record(credential_id: u64, response: &ListAvailableModelsResponse) {
    let windows: HashMap<String, i32> = response
        .models
        .iter()
        .filter_map(|model| {
            let tokens = model.token_limits.as_ref()?.max_input_tokens?;
            let tokens = i32::try_from(tokens).ok().filter(|v| *v > 0)?;
            Some((model.model_id.to_ascii_lowercase(), tokens))
        })
        .collect();
    if windows.is_empty() {
        return;
    }
    write(|limits| {
        limits.insert(credential_id, windows);
    });
}

/// 凭据删除后移除它的记录。
pub fn forget(credential_id: u64) {
    write(|limits| {
        limits.remove(&credential_id);
    });
}

/// 查询上游为该 Kiro 模型 id 声明的输入窗口（大小写不敏感）。
///
/// 多个凭据都有记录时取最大值，与 `/v1/models` 合并 `tokenLimits` 的口径一致。
pub fn max_input_tokens(model_id: &str) -> Option<i32> {
    let key = model_id.to_ascii_lowercase();
    read(|limits| {
        limits
            .values()
            .filter_map(|windows| windows.get(&key).copied())
            .max()
    })
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(models: &[(&str, Option<i64>)]) -> ListAvailableModelsResponse {
        let models: Vec<serde_json::Value> = models
            .iter()
            .map(|(id, tokens)| match tokens {
                Some(tokens) => serde_json::json!({
                    "modelId": id,
                    "tokenLimits": { "maxInputTokens": tokens }
                }),
                None => serde_json::json!({ "modelId": id }),
            })
            .collect();
        serde_json::from_value(serde_json::json!({ "models": models })).unwrap()
    }

    #[test]
    fn records_and_looks_up_case_insensitively() {
        record(1, &response(&[("Claude-Sonnet-5.5", Some(1_000_000))]));
        assert_eq!(max_input_tokens("claude-sonnet-5.5"), Some(1_000_000));
        assert_eq!(max_input_tokens("CLAUDE-SONNET-5.5"), Some(1_000_000));
        assert_eq!(max_input_tokens("claude-sonnet-9"), None);
    }

    #[test]
    fn takes_the_largest_window_across_credentials() {
        record(1, &response(&[("deepseek-3.2", Some(128_000))]));
        record(2, &response(&[("deepseek-3.2", Some(164_000))]));
        assert_eq!(max_input_tokens("deepseek-3.2"), Some(164_000));

        forget(2);
        assert_eq!(max_input_tokens("deepseek-3.2"), Some(128_000));
    }

    #[test]
    fn a_refresh_replaces_the_credential_record() {
        record(
            1,
            &response(&[("glm-5", Some(200_000)), ("kimi-k3", Some(1_000_000))]),
        );
        record(1, &response(&[("kimi-k3", Some(1_000_000))]));
        assert_eq!(max_input_tokens("glm-5"), None);
        assert_eq!(max_input_tokens("kimi-k3"), Some(1_000_000));
    }

    #[test]
    fn ignores_missing_and_invalid_limits() {
        record(1, &response(&[("auto", Some(1_000_000))]));
        // 整份响应都没有有效窗口：保留旧记录
        record(1, &response(&[("auto", None), ("bad", Some(0))]));
        assert_eq!(max_input_tokens("auto"), Some(1_000_000));

        record(
            2,
            &response(&[
                ("huge", Some(i64::from(i32::MAX) + 1)),
                ("ok", Some(256_000)),
            ]),
        );
        assert_eq!(max_input_tokens("huge"), None);
        assert_eq!(max_input_tokens("ok"), Some(256_000));
    }
}
