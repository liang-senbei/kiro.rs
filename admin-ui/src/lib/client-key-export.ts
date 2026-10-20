/**
 * 客户端 Key 导出：生成「直接使用」配置片段与 CC Switch 一键导入链接。
 *
 * CC Switch 深链接协议见其仓库 docs/guides/deeplink-site-integration-guide-zh.md：
 * `ccswitch://v1/import?resource=provider&app=claude&...`，参数驼峰命名，
 * 所有值经 URLSearchParams 转义。这里导入为 Claude Code 供应商、Anthropic 原生格式
 * （`apiFormat=anthropic`，不经 CC Switch 本地路由转换）。默认不指定模型，Claude Code
 * 用自带的模型列表；管理员选了模型时，按 Claude Code 的规则改写模型名（见
 * [`claudeCodeModelName`]），让它按上游真实窗口判断压缩。
 */

/** 客户端连接的端点：标准实时流式，或 Claude Code 兼容入口 `/cc` */
export type ExportEndpointMode = 'standard' | 'cc'

export interface ExportTarget {
  /** 客户端访问本服务的地址，如 `https://kiro.example.com` */
  baseUrl: string
  apiKey: string
  endpointMode: ExportEndpointMode
  /** 额外写给 Claude Code 的环境变量（模型槽位等），见 [`claudeCodeModelEnv`] */
  modelEnv?: Record<string, string>
  /** 连通性测试用的模型（kiro-rs 接受的 id），缺省用 Sonnet 4.5 */
  testModel?: string
}

/** `/v1/models` 里的一个模型 */
export interface ExportModel {
  /** kiro-rs 接受的模型 id：Kiro 写法（如 `claude-opus-4.8`）或自定义别名 */
  id: string
  /** 上游输入窗口（`context_window`），未知时为 0 */
  contextWindow: number
  /** `customModels` 里的别名：原样保留，不改写版本号 */
  custom?: boolean
}

/** Claude Code 的模型槽位，值为 [`ExportModel.id`]；没选的不写 */
export interface ExportModelSlots {
  main?: string
  opus?: string
  sonnet?: string
  haiku?: string
}

/** 槽位对应的 Claude Code 环境变量；CC Switch 链接里的同名参数见 [`ccSwitchImportLink`] */
export const MODEL_SLOT_ENV = {
  main: 'ANTHROPIC_MODEL',
  opus: 'ANTHROPIC_DEFAULT_OPUS_MODEL',
  sonnet: 'ANTHROPIC_DEFAULT_SONNET_MODEL',
  haiku: 'ANTHROPIC_DEFAULT_HAIKU_MODEL',
} as const satisfies Record<keyof ExportModelSlots, string>

const MAX_CONTEXT_ENV = 'CLAUDE_CODE_MAX_CONTEXT_TOKENS'
const ONE_MILLION = 1_000_000
/** Claude Code 对不认识的模型名假设的窗口 */
const CLAUDE_CODE_ASSUMED_WINDOW = 200_000

/**
 * 换成 Claude Code 认得的模型名，让它按上游真实窗口判断压缩。
 *
 * - Kiro 的点号版本（`claude-opus-4.8`）Claude Code 认不出，会当成 Opus 4 按 200K 算，
 *   改成连字符写法 `claude-opus-4-8`；kiro-rs 会映射回同一个 Kiro 模型。自定义别名原样保留。
 * - 上游 1M 的加 `[1m]`：Opus 4.6 / Sonnet 4.6、`auto`、`kimi-k3` 不加会被按 200K 压缩；
 *   Opus 4.7+ 这类原生 1M 的加上也一样，还能照顾还不认识新模型的旧版 Claude Code。
 *   Claude Code 发请求前会去掉这个后缀。
 */
export function claudeCodeModelName(
  model: ExportModel,
  /** 自定义别名 id（小写）：连字符写法撞上同名别名时会被路由到别处，这时保留原写法 */
  customIds: ReadonlySet<string> = new Set(),
): string {
  let name = model.id.trim()
  if (!model.custom) {
    const hyphen = name.replace(/^(claude-(?:opus|sonnet|haiku|fable|mythos))-(\d+)\.(\d+)$/i, '$1-$2-$3')
    if (!customIds.has(hyphen.toLowerCase())) name = hyphen
  }
  if (model.contextWindow >= ONE_MILLION && !/\[1m\]$/i.test(name)) {
    name += '[1m]'
  }
  return name
}

/**
 * 不以 `claude-` 开头、上游不到 1M 的模型（如 `deepseek-3.2` 164K、`qwen3-coder-next` 256K），
 * Claude Code 一律假设 200K：真实窗口更小时永远到不了压缩线，直接撞上游上限；更大时又压缩太早。
 * `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 告诉它真实窗口。这个变量对所有这类名字一起生效
 * （带 `[1m]` 的、Claude Code 认识的不受影响），选了多个时取最小值，宁可早压缩也不撞上限。
 */
export function claudeCodeMaxContextTokens(models: ExportModel[]): number | undefined {
  const windows = models
    .filter((m) => !/^claude-/i.test(m.id.trim()))
    .map((m) => m.contextWindow)
    .filter((w) => w > 0 && w < ONE_MILLION)
  if (!windows.length) return undefined
  const smallest = Math.min(...windows)
  return smallest === CLAUDE_CODE_ASSUMED_WINDOW ? undefined : smallest
}

/** 把选中的模型槽位换成 Claude Code 环境变量 */
export function claudeCodeModelEnv(
  slots: ExportModelSlots,
  models: ExportModel[],
): Record<string, string> {
  const byId = new Map(models.map((m) => [m.id, m]))
  const customIds = new Set(models.filter((m) => m.custom).map((m) => m.id.trim().toLowerCase()))
  const env: Record<string, string> = {}
  const picked: ExportModel[] = []
  for (const slot of Object.keys(MODEL_SLOT_ENV) as (keyof ExportModelSlots)[]) {
    const id = slots[slot]?.trim()
    if (!id) continue
    const model = byId.get(id) ?? { id, contextWindow: 0 }
    env[MODEL_SLOT_ENV[slot]] = claudeCodeModelName(model, customIds)
    picked.push(model)
  }
  const maxContext = claudeCodeMaxContextTokens(picked)
  if (maxContext !== undefined) env[MAX_CONTEXT_ENV] = String(maxContext)
  return env
}

/** 去掉首尾空白与末尾的 `/`，并去掉误填的 `/v1` 后缀（Anthropic 客户端会自己拼 `/v1/messages`） */
export function normalizeBaseUrl(raw: string): string {
  let url = raw.trim().replace(/\/+$/, '')
  url = url.replace(/\/v1$/, '')
  return url.replace(/\/+$/, '')
}

/** Anthropic 客户端的 base URL（`ANTHROPIC_BASE_URL`），不带 `/v1` */
export function anthropicBaseUrl(target: ExportTarget): string {
  const base = normalizeBaseUrl(target.baseUrl)
  return target.endpointMode === 'cc' ? `${base}/cc` : base
}

/** OpenAI 兼容客户端的 base URL（Chat Completions / Responses），带 `/v1` */
export function openAiBaseUrl(target: ExportTarget): string {
  return `${normalizeBaseUrl(target.baseUrl)}/v1`
}

/** 地址是否指向本机或内网：CC Switch 确认框会对这类地址标黄，其他机器也连不上 */
export function isLocalAddress(raw: string): boolean {
  let host: string
  try {
    host = new URL(normalizeBaseUrl(raw)).hostname
  } catch {
    return false
  }
  host = host.replace(/^\[|\]$/g, '').toLowerCase()
  if (host === 'localhost' || host.endsWith('.localhost') || host === '::1' || host === '0.0.0.0') {
    return true
  }
  const m = host.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/)
  if (!m) return false
  const [a, b] = [Number(m[1]), Number(m[2])]
  return a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168)
}

/** Claude Code `~/.claude/settings.json` 片段 */
export function claudeCodeSettingsJson(target: ExportTarget): string {
  // 只写 AUTH_TOKEN 不写 ANTHROPIC_API_KEY：两个都设时 Claude Code 会提示凭证冲突
  const settings = { env: claudeCodeEnv(target) }
  return JSON.stringify(settings, null, 2)
}

/** macOS / Linux shell 环境变量 */
export function shellExports(target: ExportTarget): string {
  return [
    ...Object.entries(claudeCodeEnv(target)).map(([k, v]) => `export ${k}=${shellQuote(v)}`),
    'claude',
  ].join('\n')
}

/** Windows PowerShell 环境变量 */
export function powershellExports(target: ExportTarget): string {
  return [
    ...Object.entries(claudeCodeEnv(target)).map(([k, v]) => `$env:${k} = ${powershellQuote(v)}`),
    'claude',
  ].join('\n')
}

/** 连通性测试：一条最小的 Messages 请求 */
export function curlTest(target: ExportTarget): string {
  const body = JSON.stringify({
    model: target.testModel?.trim() || 'claude-sonnet-4-5-20250929',
    max_tokens: 32,
    messages: [{ role: 'user', content: 'ping' }],
  })
  return [
    `curl ${shellQuote(`${anthropicBaseUrl(target)}/v1/messages`)} \\`,
    `  -H ${shellQuote(`x-api-key: ${target.apiKey}`)} \\`,
    `  -H 'anthropic-version: 2023-06-01' \\`,
    `  -H 'content-type: application/json' \\`,
    `  -d ${shellQuote(body)}`,
  ].join('\n')
}

/**
 * CC Switch 一键导入链接：Claude Code 供应商，Anthropic 原生格式。
 *
 * 链接含明文 Key，等同 Key 本身；只在用户点击时生成，不写进页面源码。
 */
export function ccSwitchImportLink(target: ExportTarget, providerName: string): string {
  const params = new URLSearchParams({
    resource: 'provider',
    app: 'claude',
    name: providerName.trim() || 'kiro-rs',
    endpoint: anthropicBaseUrl(target),
    apiKey: target.apiKey,
    apiFormat: 'anthropic',
  })
  // 模型槽位走 CC Switch 的专用参数（写成同名 ANTHROPIC_* 变量），其余变量放进 Base64 的 config.env
  const extraEnv: Record<string, string> = {}
  for (const [key, value] of Object.entries(target.modelEnv ?? {})) {
    const param = CC_SWITCH_MODEL_PARAMS[key]
    if (param) params.set(param, value)
    else extraEnv[key] = value
  }
  if (Object.keys(extraEnv).length) {
    params.set('config', base64Utf8(JSON.stringify({ env: extraEnv })))
  }
  return `ccswitch://v1/import?${params.toString()}`
}

/** 是否有模型槽位以外的变量：CC Switch 要 v3.16.0 起才保留 `config.env` 里的这些变量 */
export function hasExtraModelEnv(modelEnv: Record<string, string>): boolean {
  return Object.keys(modelEnv).some((key) => !(key in CC_SWITCH_MODEL_PARAMS))
}

/** CC Switch 深链接里与 Claude Code 模型变量对应的参数名 */
const CC_SWITCH_MODEL_PARAMS: Record<string, string> = {
  [MODEL_SLOT_ENV.main]: 'model',
  [MODEL_SLOT_ENV.opus]: 'opusModel',
  [MODEL_SLOT_ENV.sonnet]: 'sonnetModel',
  [MODEL_SLOT_ENV.haiku]: 'haikuModel',
}

/** 连接地址、Key，再加上模型等额外变量 */
function claudeCodeEnv(target: ExportTarget): Record<string, string> {
  return {
    ANTHROPIC_BASE_URL: anthropicBaseUrl(target),
    ANTHROPIC_AUTH_TOKEN: target.apiKey,
    ...target.modelEnv,
  }
}

function base64Utf8(text: string): string {
  let binary = ''
  for (const byte of new TextEncoder().encode(text)) binary += String.fromCharCode(byte)
  return btoa(binary)
}

/** 把字符串里的 Key 换成脱敏形式，用于界面展示（复制时仍用原文） */
export function maskSecret(text: string, secret: string): string {
  if (!secret) return text
  const masked =
    secret.length > 12 ? `${secret.slice(0, 6)}${'•'.repeat(8)}${secret.slice(-4)}` : '•'.repeat(8)
  return text.split(secret).join(masked)
}

function shellQuote(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`
}

function powershellQuote(value: string): string {
  return `'${value.replace(/'/g, "''")}'`
}
