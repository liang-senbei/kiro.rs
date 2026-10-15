/**
 * 客户端 Key 导出：生成「直接使用」配置片段与 CC Switch 一键导入链接。
 *
 * CC Switch 深链接协议见其仓库 docs/guides/deeplink-site-integration-guide-zh.md：
 * `ccswitch://v1/import?resource=provider&app=claude&...`，参数驼峰命名，
 * 所有值经 URLSearchParams 转义。这里导入为 Claude Code 供应商、Anthropic 原生格式
 * （`apiFormat=anthropic`，不经 CC Switch 本地路由转换）。模型参数留空：本服务认原版
 * Claude 模型名，Claude Code 用自带的模型列表即可。
 */

/** 客户端连接的端点：标准实时流式，或 Claude Code 兼容入口 `/cc` */
export type ExportEndpointMode = 'standard' | 'cc'

export interface ExportTarget {
  /** 客户端访问本服务的地址，如 `https://kiro.example.com` */
  baseUrl: string
  apiKey: string
  endpointMode: ExportEndpointMode
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
  const settings = {
    env: {
      ANTHROPIC_BASE_URL: anthropicBaseUrl(target),
      ANTHROPIC_AUTH_TOKEN: target.apiKey,
    },
  }
  return JSON.stringify(settings, null, 2)
}

/** macOS / Linux shell 环境变量 */
export function shellExports(target: ExportTarget): string {
  return [
    `export ANTHROPIC_BASE_URL=${shellQuote(anthropicBaseUrl(target))}`,
    `export ANTHROPIC_AUTH_TOKEN=${shellQuote(target.apiKey)}`,
    'claude',
  ].join('\n')
}

/** Windows PowerShell 环境变量 */
export function powershellExports(target: ExportTarget): string {
  return [
    `$env:ANTHROPIC_BASE_URL = ${powershellQuote(anthropicBaseUrl(target))}`,
    `$env:ANTHROPIC_AUTH_TOKEN = ${powershellQuote(target.apiKey)}`,
    'claude',
  ].join('\n')
}

/** 连通性测试：一条最小的 Messages 请求 */
export function curlTest(target: ExportTarget): string {
  const body = JSON.stringify({
    model: 'claude-sonnet-4-5-20250929',
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
  return `ccswitch://v1/import?${params.toString()}`
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
