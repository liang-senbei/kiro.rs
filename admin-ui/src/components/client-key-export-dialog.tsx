import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import { AlertTriangle, Copy, ExternalLink, Eye, EyeOff, Loader2, Share2 } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription,
} from '@/components/ui/dialog'
import { exportClientKey } from '@/api/client-keys'
import { cn, extractErrorMessage } from '@/lib/utils'
import {
  anthropicBaseUrl, ccSwitchImportLink, claudeCodeSettingsJson, curlTest, isLocalAddress,
  maskSecret, normalizeBaseUrl, openAiBaseUrl, powershellExports, shellExports,
  type ExportEndpointMode, type ExportTarget,
} from '@/lib/client-key-export'

/** 要导出的 Key：刚创建 / 轮换时带上明文 `key`，否则打开对话框时向服务端取回 */
export interface ClientKeyExportSubject {
  id: number
  name: string
  key?: string
  disabled?: boolean
}

type ExportTab = 'direct' | 'ccswitch'

/** 记住管理员填过的服务地址（经内网访问面板、对外是公网域名的情况） */
const BASE_URL_STORAGE_KEY = 'kiro-rs.clientKeyExport.baseUrl'

function loadBaseUrl(): string {
  try {
    const saved = localStorage.getItem(BASE_URL_STORAGE_KEY)
    if (saved) return saved
  } catch {
    // 存储不可用时退回当前地址
  }
  return window.location.origin
}

function saveBaseUrl(value: string) {
  try {
    if (normalizeBaseUrl(value) === window.location.origin) {
      localStorage.removeItem(BASE_URL_STORAGE_KEY)
    } else {
      localStorage.setItem(BASE_URL_STORAGE_KEY, value)
    }
  } catch {
    // 忽略：只是便利功能
  }
}

async function copyText(text: string, what = '已复制') {
  try {
    await navigator.clipboard.writeText(text)
    toast.success(what)
  } catch {
    toast.error('复制失败')
  }
}

function Segmented<T extends string>({
  value, options, onChange,
}: {
  value: T
  options: { value: T; label: string }[]
  onChange: (v: T) => void
}) {
  return (
    <div className="inline-flex rounded-md border border-border bg-muted/40 p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => onChange(o.value)}
          className={cn(
            'rounded px-2.5 py-1 text-[12px] transition-colors',
            value === o.value
              ? 'bg-card text-foreground shadow-xs'
              : 'text-muted-foreground hover:text-foreground',
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  )
}

function Snippet({
  title, text, display, hint,
}: {
  title: string
  /** 复制的原文（含明文 Key） */
  text: string
  /** 展示的文本（默认隐藏 Key） */
  display: string
  hint?: React.ReactNode
}) {
  return (
    <div className="space-y-1">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[12px] font-medium text-foreground">{title}</span>
        <Button
          type="button"
          size="sm"
          variant="ghost"
          className="h-6 px-2 text-[11px]"
          onClick={() => copyText(text)}
        >
          <Copy className="h-3 w-3" />
          复制
        </Button>
      </div>
      <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-all rounded-md border border-border bg-muted/40 p-2.5 font-mono text-[11px] leading-relaxed text-foreground">
        {display}
      </pre>
      {hint && <p className="text-[11px] text-muted-foreground">{hint}</p>}
    </div>
  )
}

export function ClientKeyExportDialog({
  subject, onClose,
}: {
  subject: ClientKeyExportSubject
  onClose: () => void
}) {
  const [apiKey, setApiKey] = useState<string | null>(subject.key ?? null)
  const [disabled, setDisabled] = useState(subject.disabled ?? false)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [tab, setTab] = useState<ExportTab>('direct')
  const [baseUrlInput, setBaseUrlInput] = useState(loadBaseUrl)
  const [endpointMode, setEndpointMode] = useState<ExportEndpointMode>('standard')
  const [providerName, setProviderName] = useState(`kiro-rs · ${subject.name}`)
  const [reveal, setReveal] = useState(false)

  useEffect(() => {
    if (subject.key) return
    let cancelled = false
    exportClientKey(subject.id)
      .then((res) => {
        if (cancelled) return
        setApiKey(res.key)
        setDisabled(res.disabled)
      })
      .catch((err) => {
        if (!cancelled) setLoadError(extractErrorMessage(err))
      })
    return () => {
      cancelled = true
    }
  }, [subject.id, subject.key])

  const baseUrlValid = useMemo(() => {
    try {
      const u = new URL(normalizeBaseUrl(baseUrlInput))
      return u.protocol === 'http:' || u.protocol === 'https:'
    } catch {
      return false
    }
  }, [baseUrlInput])

  const target: ExportTarget | null =
    apiKey && baseUrlValid ? { baseUrl: baseUrlInput, apiKey, endpointMode } : null
  const show = (text: string) => (reveal || !apiKey ? text : maskSecret(text, apiKey))
  const local = baseUrlValid && isLocalAddress(baseUrlInput)

  const handleImport = () => {
    if (!target) return
    // 必须在点击里同步跳转：浏览器只放行用户点击触发的外部应用唤起
    window.location.href = ccSwitchImportLink(target, providerName)
  }

  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose() }}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Share2 className="h-4 w-4 text-primary" />
            导出配置：{subject.name}
          </DialogTitle>
          <DialogDescription>
            把这把 Key 配到 Claude Code：可以直接写进配置，也可以一键导入 CC Switch（Anthropic 原生格式）。
          </DialogDescription>
        </DialogHeader>

        {loadError ? (
          <div className="rounded-md border border-destructive/30 bg-destructive/10 p-3 text-[12px] text-destructive">
            取回 Key 失败：{loadError}
          </div>
        ) : !apiKey ? (
          <div className="flex items-center gap-2 py-6 text-[12px] text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            正在取回 Key…
          </div>
        ) : (
          <div className="space-y-4">
            {disabled && (
              <div className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 p-2.5 text-[12px] text-amber-700 dark:text-amber-400">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                这把 Key 已禁用，配好后的请求会被拒绝。先在列表里启用它。
              </div>
            )}

            <div className="grid gap-3 sm:grid-cols-[1fr_auto]">
              <div>
                <label className="text-[12px] text-muted-foreground">服务地址</label>
                <Input
                  value={baseUrlInput}
                  onChange={(e) => setBaseUrlInput(e.target.value)}
                  onBlur={() => baseUrlValid && saveBaseUrl(baseUrlInput)}
                  placeholder="https://kiro.example.com"
                  className="font-mono text-[12px]"
                />
              </div>
              <div>
                <label className="text-[12px] text-muted-foreground">端点</label>
                <div>
                  <Segmented
                    value={endpointMode}
                    onChange={setEndpointMode}
                    options={[
                      { value: 'standard', label: '标准（推荐）' },
                      { value: 'cc', label: '/cc 兼容' },
                    ]}
                  />
                </div>
              </div>
            </div>
            <div className="-mt-2 space-y-1 text-[11px] text-muted-foreground">
              <p>
                客户端访问本服务的地址，默认取当前页面地址；经反向代理或域名对外提供时，填对外地址。
                {endpointMode === 'standard'
                  ? ' 标准端点实时流式输出，上游中途收尾时会透明重试。'
                  : ' /cc 端点现在和标准端点走同一条实时流，保留给已经按 /cc 配置的客户端；开头事件里的用量是估算值，结束时更正。'}
              </p>
              {!baseUrlValid && <p className="text-destructive">地址需要以 http:// 或 https:// 开头。</p>}
              {local && (
                <p className="text-amber-600 dark:text-amber-400">
                  这是本机 / 内网地址，只有能访问到它的电脑才能用；CC Switch 导入时也会对它标黄提示。
                </p>
              )}
            </div>

            <div>
              <label className="text-[12px] text-muted-foreground">API Key</label>
              <div className="relative">
                <Input
                  readOnly
                  type={reveal ? 'text' : 'password'}
                  value={apiKey}
                  className="pr-20 font-mono text-[12px]"
                />
                <div className="absolute inset-y-0 right-0 flex items-center pr-1">
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7"
                    onClick={() => setReveal((v) => !v)}
                    title={reveal ? '隐藏' : '显示'}
                  >
                    {reveal ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                  </Button>
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7"
                    onClick={() => copyText(apiKey)}
                    title="复制 Key"
                  >
                    <Copy className="h-3.5 w-3.5" />
                  </Button>
                </div>
              </div>
            </div>

            <Segmented
              value={tab}
              onChange={setTab}
              options={[
                { value: 'direct', label: '直接使用' },
                { value: 'ccswitch', label: 'CC Switch 导入' },
              ]}
            />

            {target && tab === 'direct' && (
              <div className="space-y-3">
                <Snippet
                  title="Claude Code：~/.claude/settings.json"
                  text={claudeCodeSettingsJson(target)}
                  display={show(claudeCodeSettingsJson(target))}
                  hint={
                    <>
                      合并进已有的 <code>env</code> 字段即可。只用 <code>ANTHROPIC_AUTH_TOKEN</code>，
                      不要同时设置 <code>ANTHROPIC_API_KEY</code>，否则 Claude Code 会提示凭证冲突。
                    </>
                  }
                />
                <Snippet
                  title="macOS / Linux 终端"
                  text={shellExports(target)}
                  display={show(shellExports(target))}
                />
                <Snippet
                  title="Windows PowerShell"
                  text={powershellExports(target)}
                  display={show(powershellExports(target))}
                />
                <Snippet
                  title="连通性测试"
                  text={curlTest(target)}
                  display={show(curlTest(target))}
                />
                <div className="rounded-md border border-border p-2.5 text-[11px] text-muted-foreground">
                  <div className="mb-1 font-medium text-foreground">其它客户端</div>
                  <div className="grid gap-1 sm:grid-cols-[auto_1fr] sm:gap-x-3">
                    <span>Anthropic SDK / 兼容客户端 base URL</span>
                    <code className="break-all text-foreground">{anthropicBaseUrl(target)}</code>
                    <span>OpenAI 兼容 base URL（Chat / Responses、Codex）</span>
                    <code className="break-all text-foreground">{openAiBaseUrl(target)}</code>
                  </div>
                  <p className="mt-1">Key 放在 <code>x-api-key</code> 或 <code>Authorization: Bearer</code> 头里。</p>
                </div>
              </div>
            )}

            {target && tab === 'ccswitch' && (
              <div className="space-y-3">
                <p className="text-[12px] text-muted-foreground">
                  导入为 CC Switch 里的 <strong className="text-foreground">Claude Code</strong> 供应商，接口格式
                  <strong className="text-foreground"> Anthropic 原生</strong>（<code>apiFormat=anthropic</code>）：
                  Claude Code 直接请求 <code className="break-all">{anthropicBaseUrl(target)}</code>，
                  不需要开 CC Switch 本地路由。模型留空，Claude Code 用自带的模型列表。
                </p>
                <div>
                  <label className="text-[12px] text-muted-foreground">供应商名称</label>
                  <Input value={providerName} onChange={(e) => setProviderName(e.target.value)} />
                </div>
                <div className="flex flex-wrap gap-2">
                  <Button type="button" onClick={handleImport}>
                    <ExternalLink className="h-3.5 w-3.5" />
                    导入到 CC Switch
                  </Button>
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => copyText(ccSwitchImportLink(target, providerName), '导入链接已复制')}
                  >
                    <Copy className="h-3.5 w-3.5" />
                    复制导入链接
                  </Button>
                </div>
                <div className="space-y-1 text-[11px] text-muted-foreground">
                  <p>
                    浏览器会先询问是否打开 CC Switch，选「打开」，在 CC Switch 的确认框里核对后点「导入」。
                    确认框底部的「导入后立即切换」决定是否马上用它。点了没反应？可能还没安装，先
                    {' '}
                    <a
                      href="https://ccswitch.io"
                      target="_blank"
                      rel="noopener noreferrer"
                      className="text-primary underline-offset-2 hover:underline"
                    >
                      下载 CC Switch
                    </a>
                    （建议 v4.0.10 及以上；更早的版本忽略接口格式参数，按 Claude Code 默认的 Anthropic 格式导入，同样可用）。
                  </p>
                  <p className="text-amber-600 dark:text-amber-400">
                    导入链接里带着明文 Key，等同 Key 本身，只发给这把 Key 的使用者，不要贴到公开群里。
                    同一条链接导入两次会在 CC Switch 里生成两份。
                  </p>
                </div>
              </div>
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}
